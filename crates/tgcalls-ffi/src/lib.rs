//! C ABI used by Apple clients. Every entry catches Rust unwinds. C callers must
//! still provide valid, live buffers as documented below.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::CString,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{Mutex, OnceLock},
};
use tgcalls_core::worker::Worker;
use tgcalls_isolation::wasm::{Limits, MAX_REQUEST, WasmSandbox};
#[cfg(target_os = "android")]
mod android;

enum Client {
    Native(Box<Worker>),
    Hardened(Box<WasmSandbox>),
}
#[derive(Default)]
struct Registry {
    next: u64,
    clients: BTreeMap<u64, Client>,
    outputs: BTreeMap<u64, Box<[u8]>>,
}
impl Registry {
    fn id(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1).max(1);
        self.next
    }
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
thread_local! {static ERROR:RefCell<CString>=RefCell::new(CString::default());}
fn error(message: impl ToString) {
    let message = message.to_string().replace('\0', " ");
    if let Ok(message) = CString::new(message) {
        ERROR.with(|slot| *slot.borrow_mut() = message);
    }
}
fn guard<T>(fallback: T, f: impl FnOnce() -> Result<T, String>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(message)) => {
            error(message);
            fallback
        }
        Err(_) => {
            error("Rust panic contained at FFI boundary");
            fallback
        }
    }
}

/// 0 = ordinary native worker, 1 = Wasm interpreter with no ambient capabilities.
/// iOS Hardened Mode MUST use mode 1. Module bytes are copied during this call.
///
/// # Safety
/// `module` must address `length` readable bytes when length is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tgcalls_client_new(mode: u32, module: *const u8, length: usize) -> u64 {
    guard(0, || {
        let client = match mode {
            0 => Client::Native(Box::default()),
            1 => {
                if module.is_null() || length == 0 || length > 32 * 1024 * 1024 {
                    return Err("valid Wasm module is required for Hardened Mode".into());
                }
                // SAFETY: the C caller guarantees the readable span above; only
                // this bounded slice is used during construction.
                let bytes = unsafe { std::slice::from_raw_parts(module, length) };
                Client::Hardened(Box::new(
                    WasmSandbox::new(bytes, Limits::default()).map_err(|e| e.to_string())?,
                ))
            }
            _ => return Err("unknown isolation mode".into()),
        };
        let mut registry = registry().lock().map_err(|_| "client registry poisoned")?;
        if registry.clients.len() >= 64 {
            return Err("client budget exhausted".into());
        }
        let id = registry.id();
        registry.clients.insert(id, client);
        Ok(id)
    })
}
/// Returns an owned output ticket. Read it with output_data/len, then output_free.
/// Requests are serialized. After an interpreter trap the caller must repeat
/// authenticated call negotiation; the previous call is never replayed.
///
/// # Safety
/// `bytes` must address `length` readable bytes until this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tgcalls_client_request(
    client: u64,
    bytes: *const u8,
    length: usize,
) -> u64 {
    guard(0, || {
        if bytes.is_null() || length == 0 || length > MAX_REQUEST {
            return Err("invalid request span".into());
        }
        // SAFETY: required by the C API contract; size was bounded above.
        let input = unsafe { std::slice::from_raw_parts(bytes, length) };
        let mut registry = registry().lock().map_err(|_| "client registry poisoned")?;
        if registry.outputs.len() >= 64 {
            return Err("release outstanding output tickets".into());
        }
        let current = registry.clients.get_mut(&client).ok_or("unknown client")?;
        // Catch before the registry guard can unwind: a native codec panic must
        // invalidate this client without poisoning every other call's registry.
        let result = catch_unwind(AssertUnwindSafe(|| match current {
            Client::Native(worker) => Ok(worker.execute(input)),
            Client::Hardened(worker) => worker.request(input).map_err(|e| e.to_string()),
        }));
        let output = match result {
            Ok(result) => result?,
            Err(_) => {
                registry.clients.remove(&client);
                return Err("worker panic contained; create a fresh client".into());
            }
        };
        let id = registry.id();
        registry.outputs.insert(id, output.into_boxed_slice());
        Ok(id)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_client_generation(client: u64) -> u64 {
    guard(u64::MAX, || {
        let registry = registry().lock().map_err(|_| "client registry poisoned")?;
        match registry.clients.get(&client).ok_or("unknown client")? {
            Client::Native(_) => Ok(0),
            Client::Hardened(worker) => Ok(worker.generation()),
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_client_free(client: u64) {
    guard((), || {
        registry()
            .lock()
            .map_err(|_| "client registry poisoned")?
            .clients
            .remove(&client);
        Ok(())
    })
}
/// Pointer remains valid until output_free(ticket). The caller must coordinate
/// free with concurrent readers. Unknown tickets return null.
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_output_data(ticket: u64) -> *const u8 {
    guard(ptr::null(), || {
        Ok(registry()
            .lock()
            .map_err(|_| "client registry poisoned")?
            .outputs
            .get(&ticket)
            .ok_or("unknown output")?
            .as_ptr())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_output_len(ticket: u64) -> usize {
    guard(0, || {
        Ok(registry()
            .lock()
            .map_err(|_| "client registry poisoned")?
            .outputs
            .get(&ticket)
            .ok_or("unknown output")?
            .len())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_output_free(ticket: u64) {
    guard((), || {
        registry()
            .lock()
            .map_err(|_| "client registry poisoned")?
            .outputs
            .remove(&ticket);
        Ok(())
    })
}
/// UTF-8 error text local to the calling thread, valid until its next failed call.
#[unsafe(no_mangle)]
pub extern "C" fn tgcalls_last_error() -> *const std::ffi::c_char {
    guard(
        ptr::null(),
        || Ok(ERROR.with(|slot| slot.borrow().as_ptr())),
    )
}
