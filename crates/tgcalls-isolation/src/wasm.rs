use anyhow::{Context, Result, bail};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wasmi::{
    Caller, Config, Engine, Error, Extern, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};

const MAX_MODULE: usize = 32 * 1024 * 1024;
pub const MAX_REQUEST: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const ENTROPY_BUDGET: usize = 64 * 1024;
const WASI: &str = "wasi_snapshot_preview1";
const BAD_FD: i32 = 8;
const BAD_ADDRESS: i32 = 21;
const INVALID_ARGUMENT: i32 = 28;
const IO_ERROR: i32 = 29;
const NOT_CAPABLE: i32 = 76;

#[derive(Clone, Copy)]
pub struct Limits {
    pub memory_bytes: usize,
    pub fuel: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 256 * 1024 * 1024,
            fuel: 2_000_000_000,
        }
    }
}
struct State {
    limits: StoreLimits,
    epoch: Instant,
    entropy_left: usize,
}

fn write_guest(caller: &mut Caller<'_, State>, address: i32, value: &[u8]) -> i32 {
    let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
        return BAD_ADDRESS;
    };
    match memory.write(caller, address as u32 as usize, value) {
        Ok(()) => 0,
        Err(_) => BAD_ADDRESS,
    }
}

fn entropy(mut caller: Caller<'_, State>, address: i32, length: i32) -> i32 {
    let start = address as u32 as usize;
    let length = length as u32 as usize;
    let Some(end) = start.checked_add(length) else {
        return BAD_ADDRESS;
    };
    let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
        return BAD_ADDRESS;
    };
    if end > memory.data_size(&caller) {
        return BAD_ADDRESS;
    }
    if length > caller.data().entropy_left {
        return NOT_CAPABLE;
    }
    caller.data_mut().entropy_left -= length;
    match getrandom::fill(&mut memory.data_mut(&mut caller)[start..end]) {
        Ok(()) => 0,
        Err(_) => IO_ERROR,
    }
}

fn clock(mut caller: Caller<'_, State>, id: i32, _precision: i64, address: i32) -> i32 {
    let elapsed = match id {
        0 => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
        1 => caller.data().epoch.elapsed(),
        _ => return INVALID_ARGUMENT,
    };
    let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
    write_guest(&mut caller, address, &ns.to_le_bytes())
}

fn register_imports(linker: &mut Linker<State>) -> Result<()> {
    linker.func_wrap(WASI, "random_get", entropy)?;
    linker.func_wrap(WASI, "clock_time_get", clock)?;
    linker.func_wrap(WASI, "environ_get", |_: i32, _: i32| 0)?;
    linker.func_wrap(
        WASI,
        "environ_sizes_get",
        |mut caller: Caller<'_, State>, count: i32, size: i32| {
            let result = write_guest(&mut caller, count, &[0; 4]);
            if result == 0 {
                write_guest(&mut caller, size, &[0; 4])
            } else {
                result
            }
        },
    )?;
    linker.func_wrap(WASI, "fd_close", |_: i32| BAD_FD)?;
    linker.func_wrap(WASI, "fd_fdstat_get", |_: i32, _: i32| BAD_FD)?;
    linker.func_wrap(WASI, "fd_prestat_get", |_: i32, _: i32| BAD_FD)?;
    linker.func_wrap(WASI, "fd_prestat_dir_name", |_: i32, _: i32, _: i32| BAD_FD)?;
    linker.func_wrap(WASI, "fd_write", |_: i32, _: i32, _: i32, _: i32| BAD_FD)?;
    linker.func_wrap(
        WASI,
        "path_open",
        |_: i32, _: i32, _: i32, _: i32, _: i32, _: i64, _: i64, _: i32, _: i32| NOT_CAPABLE,
    )?;
    linker.func_wrap(
        WASI,
        "proc_exit",
        |status: i32| -> std::result::Result<(), Error> { Err(Error::i32_exit(status)) },
    )?;
    linker.func_wrap(WASI, "sched_yield", || 0)?;
    Ok(())
}
struct Running {
    store: Store<State>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    request: TypedFunc<(i32, i32), i64>,
    free: TypedFunc<i32, ()>,
}

pub struct WasmSandbox {
    engine: Engine,
    module: Module,
    running: Option<Running>,
    limits: Limits,
    generation: u64,
}
impl WasmSandbox {
    pub fn new(bytes: &[u8], limits: Limits) -> Result<Self> {
        if bytes.len() > MAX_MODULE
            || limits.memory_bytes < 16 * 1024 * 1024
            || limits.memory_bytes > 512 * 1024 * 1024
            || limits.fuel == 0
        {
            bail!("invalid Wasm sandbox limits");
        }
        let mut config = Config::default();
        config.consume_fuel(true);
        // Translate to interpreter bytecode before starting any call timers.
        // This does not produce executable machine code or require JIT rights.
        config.compilation_mode(wasmi::CompilationMode::Eager);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, bytes)?;
        // Preview1 has no network imports here. The WASI context deliberately has
        // no preopened directory, inherited descriptors, arguments or environment.
        for import in module.imports() {
            if import.module() != WASI
                || !matches!(
                    import.name(),
                    "random_get"
                        | "environ_get"
                        | "environ_sizes_get"
                        | "clock_time_get"
                        | "fd_close"
                        | "fd_fdstat_get"
                        | "fd_prestat_get"
                        | "fd_prestat_dir_name"
                        | "fd_write"
                        | "path_open"
                        | "proc_exit"
                        | "sched_yield"
                )
            {
                bail!(
                    "unapproved Wasm import: {}::{}",
                    import.module(),
                    import.name()
                );
            }
        }
        let mut result = Self {
            engine,
            module,
            running: None,
            limits,
            generation: 0,
        };
        result.restart()?;
        Ok(result)
    }
    fn restart(&mut self) -> Result<()> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .memories(1)
            .tables(1)
            .instances(1)
            .table_elements(100_000)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            State {
                limits,
                epoch: Instant::now(),
                entropy_left: ENTROPY_BUDGET,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(self.limits.fuel)?;
        let mut linker = Linker::new(&self.engine);
        register_imports(&mut linker)?;
        let instance = linker.instantiate_and_start(&mut store, &self.module)?;
        if let Ok(initialize) = instance.get_typed_func::<(), ()>(&store, "_initialize") {
            initialize.call(&mut store, ())?;
        }
        self.running = Some(Running::new(store, instance)?);
        Ok(())
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn request(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        if bytes.is_empty() || bytes.len() > MAX_REQUEST {
            bail!("Wasm request size exceeds budget");
        }
        if self.running.is_none() {
            self.restart()?;
        }
        let result = self.request_inner(bytes);
        if result.is_err() {
            self.running = None;
            self.generation = self.generation.saturating_add(1);
        }
        // No state is replayed after a trap: the app must re-create/rejoin the call,
        // with new transport credentials. The next request gets a fresh instance.
        result
    }
    fn request_inner(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let run = self.running.as_mut().context("Wasm instance unavailable")?;
        run.store.data_mut().entropy_left = ENTROPY_BUDGET;
        run.store.set_fuel(self.limits.fuel)?;
        let input = run.alloc.call(&mut run.store, bytes.len() as i32)?;
        if input == 0 {
            bail!("Wasm allocation refused");
        }
        run.memory
            .write(&mut run.store, input as u32 as usize, bytes)?;
        let result = run
            .request
            .call(&mut run.store, (input, bytes.len() as i32))? as u64;
        run.free.call(&mut run.store, input)?;
        let pointer = (result >> 32) as u32;
        let length = (result & 0xffff_ffff) as usize;
        if pointer == 0 || length == 0 || length > MAX_RESPONSE {
            bail!("invalid Wasm response span");
        }
        let mut output = vec![0; length];
        run.memory.read(&run.store, pointer as usize, &mut output)?;
        run.free.call(&mut run.store, pointer as i32)?;
        Ok(output)
    }
}
impl Running {
    fn new(store: Store<State>, instance: Instance) -> Result<Self> {
        Ok(Self {
            memory: instance
                .get_memory(&store, "memory")
                .context("missing Wasm memory")?,
            alloc: instance.get_typed_func(&store, "tg_alloc")?,
            request: instance.get_typed_func(&store, "tg_request")?,
            free: instance.get_typed_func(&store, "tg_free")?,
            store,
        })
    }
}
