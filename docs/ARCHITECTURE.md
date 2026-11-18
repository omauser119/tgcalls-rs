# Архитектура

Схема ниже — существующий Linux/Python аудиотракт. Новый `sansio::Engine`
использует тот же `rtc` protocol core без собственного сетевого рантайма.
`worker::Worker` объединяет DH, защищённый личный сигналинг и Opus/H.264 с
ограниченным request API. C ABI в `tgcalls-ffi` используется Swift/Android SDK.
На iOS тот же Worker компилируется в WASI reactor и исполняется Wasmi без JIT;
на Android его размещает isolated Service, на macOS — sandboxed XPC Service.
Подробные границы, сборки и текущие ограничения: [MOBILE.md](MOBILE.md).

```
Приложение / PyTGCalls / Telethon (MTProto, авторизация)
  └── PyO3 Engine
       ├── standard: Session → webrtc-rs / Tokio → RustyOpus
       └── hardened: Supervisor
            ├── приватная UnixStream::pair: JSON control, max 64 KiB
            ├── SCM_RIGHTS: sealed memfd, 2 prebound UDP sockets, 2 eventfd
            ├── memfd: input/output rings, 32 slots × 23040 bytes
            └── exec worker → single-thread Tokio → seccomp TSYNC → Session
```

На отправке PyO3 заимствует срез неизменяемого Python `bytes`. Обычный режим
передаёт его кодеру. Готовая операция возвращает RTP timestamp сразу, а при
обратном давлении та же операция продолжается в Tokio вместе с владельцем
буфера. Управляющие вызовы и ожидание приёма используют `asyncio.to_thread`.

В Harden Mode supervisor записывает срез прямо в кольцо, когда управляющий
lock свободен. Helper читает кадр из атомарных слов в переиспользуемый буфер.
На этой границе выполняются две атомарные копии. PCM не сериализуется в JSON,
и ядро не переносит его по управляющему сокету. Кодек преобразует PCM в Opus,
а API приёма возвращает принадлежащие Python `bytes`.

Каждый eventfd будит читателя только после публикации кадра. Уведомления могут
объединяться. Буфер задаёт порядок и число кадров, поэтому счётчик eventfd не
используется для индексации или выделения памяти. Выпуск аудио планируется по
длительности декодированного кадра и текущей задержке jitter buffer.

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
reorder window 128 пакетами и выбирает 20–200 мс буферизации. База составляет
длительность одного кадра, к ней добавляется запас по джиттеру. Рост оценки задерживает
следующий playout, уменьшение сбрасывает ограниченное число кадров; time stretching
пока нет. Opus PLC при потерях использует последнюю известную длительность пакета.
На входе поддержаны Opus packets до 120 ms. При изменении packet duration именно
во время потери точная длительность неизвестна до следующего принятого кадра.
