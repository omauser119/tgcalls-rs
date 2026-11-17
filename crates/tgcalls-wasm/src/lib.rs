//! WASI reactor: the complete Rust protocol/codec worker lives in Wasm memory.
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};
use tgcalls_core::worker::{MAX_REQUEST, Worker};

struct Runtime {
    worker: Worker,
    allocations: BTreeMap<u32, Box<[u8]>>,
}
fn runtime() -> &'static Mutex<Runtime> {
    static INSTANCE: OnceLock<Mutex<Runtime>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        Mutex::new(Runtime {
            worker: Worker::default(),
            allocations: BTreeMap::new(),
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn tg_alloc(length: u32) -> u32 {
    if length == 0 || length as usize > MAX_REQUEST {
        return 0;
    }
    let Ok(mut runtime) = runtime().lock() else {
        return 0;
    };
    if runtime.allocations.len() >= 4 {
        return 0;
    }
    let mut allocation = vec![0; length as usize].into_boxed_slice();
    let pointer = allocation.as_mut_ptr() as u32;
    runtime.allocations.insert(pointer, allocation);
    pointer
}
#[unsafe(no_mangle)]
pub extern "C" fn tg_free(pointer: u32) {
    if let Ok(mut runtime) = runtime().lock() {
        runtime.allocations.remove(&pointer);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tg_request(pointer: u32, length: u32) -> u64 {
    let Ok(mut runtime) = runtime().lock() else {
        return 0;
    };
    let Some(input) = runtime
        .allocations
        .get(&pointer)
        .and_then(|v| v.get(..length as usize))
        .map(<[u8]>::to_vec)
    else {
        return 0;
    };
    let output = runtime.worker.execute(&input);
    if output.len() > 32 * 1024 * 1024 {
        return 0;
    }
    let mut output = output.into_boxed_slice();
    let pointer = output.as_mut_ptr() as u32;
    let length = output.len() as u32;
    runtime.allocations.insert(pointer, output);
    (u64::from(pointer) << 32) | u64::from(length)
}
