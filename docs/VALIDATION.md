# Проверка реализации

Среда: Linux x86_64, rustc 1.98.1, CPython 3.14.8. Дата: 2026-11-17.

Локальные проверки:

* `cargo fmt --all -- --check`
* `cargo clippy --locked --workspace --all-targets -- -D warnings`
* `cargo test --locked --workspace`: 12 функциональных тестов
* PyO3 abi3 wheel: сборка через `maturin develop --locked`
* `pytest -q`: 11 тестов, PCM standard/hardened, async API, timeout/input validation,
  actual ntgcalls types, PyTGCalls external audio adapter, frame callback types
* Harness: только `--list`, импорт и чистые функции; performance scenarios не запускались

Ключевые Rust-тесты используют настоящий ICE/DTLS/SRTP, RustyOpus decode и
ICE-lite SFU с Telegram DataJSON. Изоляция проверяется в реальном subprocess:
seccomp/NoNewPrivs/CapEff, заблокированные filesystem/exec/io_uring syscalls,
sealed-memory truncate/overflow, SIGKILL recovery и смена Telegram credentials.

`cargo-audit 0.23.0`: 231 зависимость, **0 известных уязвимостей, 0 warnings**.
RustSec database: commit `233155b4025b854c68a4b199c0e32d7fd53291a2`,
updated `2026-11-17T11:14:35+01:00`, 1527 advisories. Это проверка известных
advisories для версий зависимостей, не независимый аудит собственного кода,
вендорного патча или git-ревизии RustyOpus.

Не выполнялись: live-звонок Telegram, нагрузочные/RTT/CPU/RSS бенчмарки,
kernel netem scenarios, длительный fuzzing и release performance qualification.
Для live-проверки нужны явно указанные пользовательская сессия и тестовый чат.
Полный паритет с оригинальным TgCalls не подтверждён; unsupported matrix
зафиксирован в [COMPATIBILITY.md](COMPATIBILITY.md).
