# Telegram / PyTGCalls compatibility

Обязательный протокольный путь: сгенерировать локальные ICE credentials,
SHA-256 DTLS fingerprint и SSRC → Telegram `phone.joinGroupCall(params=DataJSON)`
→ `UpdateGroupCallConnection.params` → full ICE на Linux → DTLS active
на стороне Telegram, passive на стороне клиента → SRTP Opus/PT111.

В Linux-аудиотракте клиент начинает ICE в роли `controlled`. Он применяет
параметры SFU как локально синтезированный SDP offer и сохраняет проверку
DTLS fingerprint. Используемые в `phone.joinGroupCall` локальные credentials
и fingerprint предварительно извлекаются из черновика SDP. Мобильный
host-driven Worker пока использует прежний путь через SDP answer.

Формат сверялся с оригинальными исходниками:

* [Telegram GroupJoinPayloadInternal.cpp](https://github.com/TelegramMessenger/tgcalls/blob/development/tgcalls/group/GroupJoinPayloadInternal.cpp)
* [Telegram GroupInstanceReferenceImpl.cpp](https://github.com/TelegramMessenger/tgcalls/blob/development/tgcalls/group/GroupInstanceReferenceImpl.cpp)
* [PyTGCalls connect_call.py](https://github.com/pytgcalls/pytgcalls/blob/master/pytgcalls/methods/internal/connect_call.py)

В приватной обычной группе с единственным участником проверены вход в живой
звонок, отправка PCM и сохранение SSRC в Telegram. Двусторонний звук с
официальными клиентами, смена участников и rejoin после сбоя в разных сетях
остаются отдельными сценариями проверки.

NTgCalls-compatible subset: `create_call`, `connect`, `set_stream_sources`
(ExternalMedia.AUDIO only), `send_external_frame`, `stop`, `calls`, `get_state`,
`get_connection_mode`, `get_call_type`, `update_audio_ssrc_mappings`, mute/unmute.
API имена и типы сверяются с установленными PyTGCalls 3.0.1 / ntgcalls 3.0.7.
PyTGCalls использует private `_binding`; обновление версии требует повторной
проверки адаптера. Отсутствующие методы не подменяются успешными заглушками.

Rust core теперь реализует личные звонки: проверенный DH exchange, Telegram V2
encrypted signaling (8.0.0/9.0.0), однонаправленные медиаканалы в обе стороны,
Opus и Baseline H.264 на Remade-With-Rust. Есть локальные тесты полного обмена
ключами/сигналингом и двустороннего медиатракта. Core API и mobile request API
пока не подключены к личным звонкам/видео в старом Python NTgCalls-адаптере.
В Python отдельный `TelegramPersonalCall` связывает Worker с Telethon. Мобильный
транспорт поддерживает стандартные UDP STUN и TURN. Хост передаёт локальный адрес
нового сокета для ICE restart. Нужна проверка перехода Wi-Fi/LTE и личных звонков
с официальным клиентом.
V3 signaling (версии 12/13), Telegram reflector/MTProto media, групповое видео,
simulcast, conference E2EE, broadcast transport и device/file/shell media sources
не реализованы. DTLS/SRTP группового звонка не следует называть Telegram E2EE.
Поэтому полный функциональный и качественный паритет с оригинальным TgCalls
на текущем этапе **не достигнут**.

Apple/Android сборки и точные границы Harden Mode: [MOBILE.md](MOBILE.md).
