# Модель угроз и границы защиты

Проект экспериментальный; аудит и fuzzing зависимостей не завершены. Наличие
Rust, catch_unwind и seccomp само по себе не доказывает безопасность.

Ниже описан Linux/Python helper. Платформенные политики Android isolated UID,
macOS XPC и iOS Wasm-интерпретатора описаны в [MOBILE.md](MOBILE.md). Для iOS
граница проходит по линейной памяти и capability API внутри процесса: guest
panic/abort, выход за память и исчерпание fuel перехватываются как traps. Хост,
интерпретатор и C ABI остаются доверенными. Гарантии отдельного OS-процесса на
iOS не заявляются. Нельзя включать JIT или переходить в native mode при ошибке.

Приложение передаёт в гостя зашифрованный сетевой трафик, локальный PCM и
проверенный ключ медиа личного звонка. MTProto-сессия и ключи групповой
E2E-конференции остаются в приложении. Сетевой брокер принимает адреса SFU
из ответа Telegram и адреса TURN из параметров звонка. При сбое гостя
активная сессия закрывается, а повторный вход требует новых параметров ICE.

В конференции Rust передаёт закодированный кадр доверенному `tde2e`
контексту. Исходящий кадр получает сетевой RTP после шифрования. Входящий
передаётся в декодер после проверки подлинности. Неизвестный источник,
ошибка проверки и просроченный запрос трансформации ведут к отбрасыванию
кадра. H.264 оставляет в открытом виде необходимые SFU заголовки, остальная
часть кадра защищается механизмом Telegram. Доступность гостя и пределы
его использования CPU проверяются отдельно от шифрования.
[Спецификация E2E Telegram](https://core.telegram.org/api/end-to-end/group-calls)

Обычный режим получает Result-based проверки размеров SDP/JSON/RTP/Opus,
ограниченные очереди и catch_unwind на Python FFI и собственных codec tasks.
Необработанная паника или abort в зависимости может оборвать in-process runtime;
OOM и SIGSEGV catch_unwind не перехватывает. Release профиль сохраняет unwind и
проверки integer overflow.

В Hardened Mode сетевой стек, SDP/JSON/RTP/Opus parsing и codec находятся в worker.
Фильтр устанавливается до создания peer connection и получения сетевых пакетов.
Фильтр — allowlist с архитектурной проверкой и TSYNC. Неизвестные syscalls дают
EPERM. `open/openat/openat2`, unlink/rename/mkdir, exec/fork/clone, ptrace,
io_uring, создание AF_UNIX/raw sockets не разрешены. Новые Internet UDP sockets
разрешены для WebRTC; helper не является сетевой песочницей, запрещающей
исходящие UDP-пакеты. Файловые syscalls не разрешают обход фильтра через пути.

Worker сбрасывает effective/permitted/inheritable/ambient capabilities и использует
no_new_privs, dumpable=0, RLIMIT_CORE=0, RLIMIT_AS=1 GiB,
RLIMIT_NOFILE=256 и parent-death SIGKILL. Он наследует uid родителя; запуск
приложения от root не рекомендуется. seccomp не заменяет отдельный uid/userns,
контейнер или защиту от kernel exploits.

Управляющий сокет создаётся через socketpair без filesystem pathname.
Bootstrap принимает ровно пять SCM_RIGHTS descriptors с CLOEXEC: sealed memfd,
два UDP-сокета и два eventfd для уведомлений о PCM. При ошибке дескрипторы
закрываются. IPC frames ограничены 64 KiB до выделения памяти; от worker не принимаются
произвольные пути или команды. Helper выбирает доверенное приложение по явному
абсолютному пути; защита от подмены этого бинарника — обязанность установки.

Компрометация worker может раскрыть текущий PCM, ICE/DTLS ключи и повлиять на
аудио. Она не должна давать чтение файлов или доступ к MTProto session в памяти
родителя. Parent читает только ограниченные JSON ответы и атомарные PCM rings.
Секреты Telegram остаются в Python MTProto-клиенте, вне worker.

Watchdog ограничивает последствия сбоев, но не гарантирует непрерывный звонок и
не исключает denial of service. При невозможности установить seccomp, плохом
IPC, повреждении shared memory или исчерпании restart budget применяется
fail-closed поведение. Поддержка аrchitectures кроме Linux x86_64 требует
отдельного тестирования syscall policy.
