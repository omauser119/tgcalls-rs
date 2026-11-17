use anyhow::{Context, Result, bail};
use wasmi::{
    Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc,
};
use wasmi_wasi::{WasiCtx, WasiCtxBuilder};

const MAX_MODULE: usize = 32 * 1024 * 1024;
pub const MAX_REQUEST: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 32 * 1024 * 1024;

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
    wasi: WasiCtx,
    limits: StoreLimits,
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
            if import.module() != "wasi_snapshot_preview1"
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
        let wasi = WasiCtxBuilder::new().build();
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .memories(1)
            .tables(1)
            .instances(1)
            .table_elements(100_000)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&self.engine, State { wasi, limits });
        store.limiter(|state| &mut state.limits);
        store.set_fuel(self.limits.fuel)?;
        let mut linker = Linker::new(&self.engine);
        wasmi_wasi::add_to_linker(&mut linker, |state: &mut State| &mut state.wasi)?;
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
