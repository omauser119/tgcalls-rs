# tgcalls-rs

Экспериментальный Rust-движок Telegram: **webrtc-rs + Tokio +
[rusty-opus](https://github.com/remade-with-rust/rusty-opus)**, PyO3 и отдельный
Linux worker с seccomp. Личные звонки и видео реализованы в Rust core; кодеки —
**RustyOpus и [Remade-With-Rust/rusty_h264](https://github.com/Remade-With-Rust/rusty_h264)**.
Системные libopus, libx264 и FFmpeg не используются. Для Apple/Android добавлен
host-driven клиент и платформенные границы изоляции: [MOBILE.md](docs/MOBILE.md).

Это начальная реализация, **не полный и не сертифицированный по безопасности
заменитель оригинального TgCalls**. Есть Telegram DataJSON/SDP bridge и примеры
реального MTProto-подключения; live-совместимость с Telegram требует проверки
авторизованной сессией. Локальные тесты не заменяют такую проверку.

| Возможность | Состояние |
| --- | --- |
| Настоящий ICE/DTLS/SRTP loopback, RustyOpus encode/decode | Реализовано |
| Ограниченный адаптивный jitter buffer, PLC, PCM 48 kHz stereo | Реализовано |
| `phone.joinGroupCall` JSON, fingerprint/ICE, удалённые audio SSRC | Реализовано; live Telegram пока не проверен |
| PyTGCalls 3.0.1: групповой `ExternalMedia.AUDIO` backend | Адаптер; не полный NTgCalls API |
| Отдельный worker, SCM_RIGHTS, sealed memfd, seccomp, watchdog | Реализовано для Linux |
| Автоматизированные RTT/CPU/RSS/loss/jitter сценарии | Harness подготовлен; бенчмарки не запускались |
| Личные звонки: DH, V2 encrypted signaling, двусторонние Opus/H.264 | Core и mobile request API, локальные тесты; live Telegram не проверен |
| iOS Harden Mode | Wasm-интерпретатор без JIT, лимиты памяти/fuel, сброс после trap; функциональный медиатест |
| macOS / Android Harden Mode | Swift XPC / Java isolated Service + SharedMemory; сборки в CI, проверка на устройствах требуется |
| Групповое видео, simulcast, RTMP/MTProto broadcasts | Не реализованы |
| AEC/NS/AGC, микрофон/динамик, resampling | Не реализованы; приложение подаёт готовый PCM |

## Сборка

Linux x86_64, Rust stable, Python 3.11+. ABI модуля — PyO3 abi3-py311.

```bash
python3 -m venv .venv
. .venv/bin/activate
pip install 'maturin>=1.9,<2'
cargo build --locked --release -p tgcalls-sandbox
maturin develop --locked --release
export TGCALLS_SANDBOX_BIN="$PWD/target/release/tgcalls-sandbox"
```

Helper поставляется отдельно от Python wheel. Hardened Mode требует явного
абсолютного пути к нему и завершает запуск ошибкой, если изоляция недоступна.
Автоматического перехода в обычный режим нет.

## Python

```python
from tgcalls_rs import Call

async def send_audio(pcm_frames):
    async with Call(hardened=True) as call:
        await call.start_loopback()
        for pcm in pcm_frames:       # bytes: 3840 байт = 20 ms stereo s16le @48kHz
            await call.send_pcm(pcm)
            frame = await call.receive(timeout=1)
            consume(frame.pcm)
```

Потоки нужно подавать с интервалом 20 ms. Возвращаемый RTP timestamp позволяет
сопоставлять отправленные и принятые кадры. `Call(hardened=False)` использует тот
же медиатракт в основном процессе. Режим выбирается для нового соединения.

## Telegram

```bash
pip install -e '.[telegram]'
export API_ID=... API_HASH=... TG_CHAT=-100...
python examples/telegram_group.py audio.s16le --hardened \
  --helper "$TGCALLS_SANDBOX_BIN"
```

Нужна пользовательская Telegram-сессия и уже открытый голосовой чат. Пример
выполняет настоящий `phone.joinGroupCall`, применяет ответ Telegram и отправляет
PCM. Секреты и `.session` файлы не коммитятся. Вызов примера связывается с Telegram
и передаёт звук, поэтому он не выполняется тестами или CI.

Для PyTGCalls: `pip install -e '.[pytgcalls,telegram]'` и
[examples/pytgcalls_external.py](examples/pytgcalls_external.py). Установите backend
до `PyTgCalls.start()`. На текущем этапе поддержан внешний PCM; неподдержанные
видео, device/file/shell sources отклоняются явно. PyTGCalls по-прежнему импортирует
типы из ntgcalls, но выбранные audio-вызовы исполняет Rust backend.

## Проверки и бенчмарки

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pip install -e '.[test,pytgcalls,telegram]'
TGCALLS_SANDBOX_BIN="$PWD/target/debug/tgcalls-sandbox" pytest -q
```

Тесты проверяют поведение и отказоустойчивость, не собирают показатели
производительности. Матрицу бенчмарков можно просмотреть без их запуска:

```bash
python benchmarks/run.py --list
```

Ручной запуск после release-сборки:

```bash
pip install -e '.[benchmark]'
python benchmarks/run.py --mode both --calls 4 --seconds 30 \
  --helper "$TGCALLS_SANDBOX_BIN" --output results/comparison.json
```

Подробности: [архитектура](docs/ARCHITECTURE.md), [модель угроз](docs/SECURITY.md),
[методика измерений](docs/BENCHMARKS.md), [совместимость](docs/COMPATIBILITY.md).
