fn main() {
    let result = std::panic::catch_unwind(|| {
        if std::env::args().nth(1).as_deref() == Some("--self-test-seccomp") {
            tgcalls_sandbox::worker::policy_self_test()
        } else {
            tgcalls_sandbox::worker::run()
        }
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("tgcalls worker: {error:#}");
            std::process::exit(1);
        }
        Err(_) => std::process::exit(70),
    }
}
