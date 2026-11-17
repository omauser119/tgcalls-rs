# Telegram / PyTGCalls compatibility

Обязательный протокольный путь: сгенерировать локальные ICE credentials,
SHA-256 DTLS fingerprint и SSRC → Telegram `phone.joinGroupCall(params=DataJSON)`
→ `UpdateGroupCallConnection.params` → ICE-lite remote answer → DTLS active
на стороне Telegram, passive на стороне клиента → SRTP Opus/PT111.

Формат сверялся с оригинальными исходниками:

* [Telegram GroupJoinPayloadInternal.cpp](https://github.com/TelegramMessenger/tgcalls/blob/development/tgcalls/group/GroupJoinPayloadInternal.cpp)
* [Telegram GroupInstanceReferenceImpl.cpp](https://github.com/TelegramMessenger/tgcalls/blob/development/tgcalls/group/GroupInstanceReferenceImpl.cpp)
* [PyTGCalls connect_call.py](https://github.com/pytgcalls/pytgcalls/blob/master/pytgcalls/methods/internal/connect_call.py)

Это не доказательство live-совместимости. В репозитории нет Telegram credentials,
а локальный WebRTC peer и synthetic DataJSON fixture не являются Telegram SFU.
Перед заявлением production compatibility нужен реальный групповой звонок в
обоих режимах, двусторонний звук с официальными клиентами, join/leave, смена
участников, rejoin после worker crash и разные сети.

NTgCalls-compatible subset: `create_call`, `connect`, `set_stream_sources`
(ExternalMedia.AUDIO only), `send_external_frame`, `stop`, `calls`, `get_state`,
`get_connection_mode`, `get_call_type`, `update_audio_ssrc_mappings`, mute/unmute.
API имена и типы сверяются с установленными PyTGCalls 3.0.1 / ntgcalls 3.0.7.
PyTGCalls использует private `_binding`; обновление версии требует повторной
проверки адаптера. Отсутствующие методы не подменяются успешными заглушками.

Нет поддержки Telegram personal calls (DH auth_key и encrypted signaling),
video/simulcast, conference E2EE, MTProto broadcast transport, device/file/shell
media sources. DTLS/SRTP группового звонка не следует называть Telegram E2EE.
Поэтому полный функциональный и качественный паритет с оригинальным TgCalls
на текущем этапе **не достигнут**.
