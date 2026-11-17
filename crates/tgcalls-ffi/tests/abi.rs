use tgcalls_ffi::*;
#[test]
fn c_abi_owns_outputs_and_refuses_invalid_handles() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: native creation ignores the null module span.
    let handle = unsafe { tgcalls_client_new(0, std::ptr::null(), 0) };
    assert_ne!(handle, 0);
    let input = br#"{"op":"new","local":"127.0.0.1:43000","ssrc":1000}"#;
    // SAFETY: the static byte slice is alive and readable throughout this call.
    let output = unsafe { tgcalls_client_request(handle, input.as_ptr(), input.len()) };
    assert_ne!(output, 0);
    let pointer = tgcalls_output_data(output);
    let length = tgcalls_output_len(output);
    assert!(!pointer.is_null());
    // SAFETY: this ticket is live and has not been freed; the API owns its buffer.
    let response: serde_json::Value =
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(pointer, length) })?;
    assert_eq!(response["ok"], true);
    tgcalls_output_free(output);
    assert_eq!(tgcalls_output_len(output), 0);
    tgcalls_client_free(handle);
    // SAFETY: the buffer is valid; the stale handle must be rejected.
    let stale = unsafe { tgcalls_client_request(handle, input.as_ptr(), input.len()) };
    assert_eq!(stale, 0);
    // SAFETY: the null/empty module must fail closed before dereferencing it.
    assert_eq!(unsafe { tgcalls_client_new(1, std::ptr::null(), 0) }, 0);
    Ok(())
}
