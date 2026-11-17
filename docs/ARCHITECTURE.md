# Архитектура

```
Приложение / PyTGCalls / Telethon (MTProto, авторизация)
  └── asyncio.to_thread → PyO3 Engine
       ├── standard: Session → webrtc-rs / Tokio → RustyOpus
       └── hardened: Supervisor
            ├── приватная UnixStream::pair: JSON control, max 64 KiB
            ├── SCM_RIGHTS: sealed memfd + 2 prebound UDP sockets
            ├── memfd: input/output rings, 32 slots × 23040 bytes
            └── exec worker → single-thread Tokio → seccomp TSYNC → Session
```

PCM идёт через shared memory, без сериализации аудио в JSON и без копирования
между адресными пространствами ядром. Это **не zero-overhead и не end-to-end
zero-copy**: остаются копии Python/Rust, атомарные копии в ring и работа кодека.
Величину накладных расходов должен определить benchmark harness.

Все разделяемые поля, включая payload, атомарные. Повреждённые счётчики и длины
проверяются до индексации. memfd запечатан от изменения размера, чтобы другая
сторона не могла вызвать SIGBUS посредством truncate. Фиксированные лимиты
действуют и при компрометации worker. Ring SPSC; Python send/receive locks и
worker media tasks сериализуют соответствующих производителей/потребителей.

Вендорный webrtc 0.21.1 содержит небольшой патч `with_prebound_udp_sockets`.
Он необходим, чтобы ICE объявлял адрес **переданного** socket. Подмена socket
через runtime hook после bind оставляет в upstream driver другой local_addr.
Патч не меняет ICE/DTLS/SRTP протоколы. Источник и diff указаны в
[VENDORED-WEBRTC.md](VENDORED-WEBRTC.md).

Перезапуск worker не восстанавливает старые ICE/DTLS ключи. Loopback создаётся
заново. Telegram получает новый join payload и флаг `needs_rejoin`; Python
Telethon bridge делает повторный `phone.joinGroupCall`. До rejoin медиапередача
отклоняется. PyTGCalls adapter пока требует обработки rejoin приложением.

Watchdog проверяет worker раз в 100 ms; idle heartbeat timeout — 500 ms.
Control RPC имеет отдельный таймаут до 20 s и сериализуется с heartbeat.
Нельзя обещать «мгновенный» перезапуск: занятый RPC может задержать обнаружение
зависания до своего таймаута. Бюджет 5 попыток в минуту ограничивает crash loops.

Адаптивный jitter buffer оценивает RFC3550 interarrival jitter, ограничивает
reorder window 128 пакетами и выбирает 40–200 ms buffering. Рост оценки задерживает
следующий playout, уменьшение сбрасывает ограниченное число кадров; time stretching
пока нет. Opus PLC при потерях использует последнюю известную длительность пакета.
На входе поддержаны Opus packets до 120 ms. При изменении packet duration именно
во время потери точная длительность неизвестна до следующего принятого кадра.
