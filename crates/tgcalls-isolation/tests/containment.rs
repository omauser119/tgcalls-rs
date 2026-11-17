use tgcalls_isolation::wasm::{Limits, WasmSandbox};

fn module(body: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 1)
        (func (export "tg_alloc") (param i32) (result i32) i32.const 1024)
        (func (export "tg_free") (param i32))
        (func (export "tg_request") (param i32 i32) (result i64) {body})
    )"#
    ))?)
}
#[test]
fn trap_discards_instance_and_restarts_without_killing_host()
-> Result<(), Box<dyn std::error::Error>> {
    let mut sandbox = WasmSandbox::new(&module("unreachable")?, Limits::default())?;
    for generation in 1..=2 {
        assert!(sandbox.request(b"{}").is_err());
        assert_eq!(sandbox.generation(), generation);
    }
    Ok(())
}
#[test]
fn fuel_stops_infinite_loop() -> Result<(), Box<dyn std::error::Error>> {
    let mut sandbox = WasmSandbox::new(
        &module("(loop $forever br $forever) i64.const 0")?,
        Limits {
            fuel: 10_000,
            ..Limits::default()
        },
    )?;
    assert!(sandbox.request(b"{}").is_err());
    assert_eq!(sandbox.generation(), 1);
    Ok(())
}
#[test]
fn linear_memory_growth_and_bad_spans_are_contained() -> Result<(), Box<dyn std::error::Error>> {
    for body in [
        "i32.const 8192 memory.grow drop i64.const 0",
        "i64.const -1",
    ] {
        let mut sandbox = WasmSandbox::new(
            &module(body)?,
            Limits {
                memory_bytes: 16 * 1024 * 1024,
                ..Limits::default()
            },
        )?;
        assert!(sandbox.request(b"{}").is_err());
        assert_eq!(sandbox.generation(), 1);
    }
    Ok(())
}
#[test]
fn ambient_network_import_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = wat::parse_str(r#"(module (import "host" "network" (func)))"#)?;
    assert!(WasmSandbox::new(&bytes, Limits::default()).is_err());
    Ok(())
}

#[test]
fn guest_process_exit_cannot_exit_the_host() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = wat::parse_str(
        r#"(module
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (func (export "tg_alloc") (param i32) (result i32) i32.const 1024)
        (func (export "tg_free") (param i32))
        (func (export "tg_request") (param i32 i32) (result i64) i32.const 70 call $exit i64.const 0))"#,
    )?;
    let mut sandbox = WasmSandbox::new(&bytes, Limits::default())?;
    assert!(sandbox.request(b"{}").is_err());
    assert_eq!(sandbox.generation(), 1);
    Ok(())
}

#[test]
fn wasi_context_has_no_preopened_app_directory() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = wat::parse_str(
        r#"(module
        (import "wasi_snapshot_preview1" "fd_prestat_get" (func $prestat (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 2048) "{}")
        (func (export "tg_alloc") (param i32) (result i32) i32.const 1024)
        (func (export "tg_free") (param i32))
        (func (export "tg_request") (param i32 i32) (result i64)
            i32.const 3 i32.const 4096 call $prestat i32.const 8 i32.ne
            if unreachable end
            i64.const 8796093022210))"#,
    )?;
    let mut sandbox = WasmSandbox::new(&bytes, Limits::default())?;
    assert_eq!(sandbox.request(b"{}")?, b"{}");
    Ok(())
}
