use super::*;
use jni::{
    JNIEnv,
    objects::{JByteArray, JClass},
    sys::{jbyteArray, jlong},
};
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tgcalls_rust_NativeBridge_create(
    mut env: JNIEnv,
    _: JClass,
) -> jlong {
    guard(0, || {
        // SAFETY: native mode does not dereference the module pointer.
        let handle = unsafe { tgcalls_client_new(0, ptr::null(), 0) };
        if handle == 0 {
            let _ = env.throw_new(
                "java/lang/IllegalStateException",
                "native worker unavailable",
            );
        }
        Ok(handle as jlong)
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tgcalls_rust_NativeBridge_request(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    input: JByteArray,
) -> jbyteArray {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<jbyteArray, String> {
        let bytes = env.convert_byte_array(input).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_REQUEST {
            return Err("request budget exceeded".into());
        }
        // SAFETY: the Vec keeps this bounded span alive for the duration of the call.
        let ticket = unsafe { tgcalls_client_request(handle as u64, bytes.as_ptr(), bytes.len()) };
        if ticket == 0 {
            return Err("worker request failed".into());
        }
        let length = tgcalls_output_len(ticket);
        let pointer = tgcalls_output_data(ticket);
        let result = if pointer.is_null() {
            Err("invalid worker response".into())
        } else {
            // SAFETY: the output ticket owns this span; it is freed after copying.
            let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
            env.byte_array_from_slice(bytes)
                .map(|a| a.into_raw())
                .map_err(|e| e.to_string())
        };
        tgcalls_output_free(ticket);
        result
    }));
    match result {
        Ok(Ok(array)) => array,
        other => {
            let message = match other {
                Ok(Err(e)) => e,
                _ => "Rust panic contained at JNI boundary".into(),
            };
            let _ = env.throw_new("java/lang/IllegalStateException", message);
            ptr::null_mut()
        }
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tgcalls_rust_NativeBridge_destroy(
    _env: JNIEnv,
    _: JClass,
    handle: jlong,
) {
    guard((), || {
        tgcalls_client_free(handle as u64);
        Ok(())
    })
}
