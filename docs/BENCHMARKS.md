# Методика бенчмарков

[Замеры оптимизации Linux-аудиотракта](results/README.md)

`benchmarks/run.py` создаёт реальные WebRTC peer connections. Сравнивает один и
тот же медиатракт в standard/hardened режиме, несколько одновременных звонков,
разогрев и повторения с чередованием порядка режимов.

* Setup: время до ICE/DTLS Connected; в hardened включает exec, shared memory и seccomp.
* Digital media RTT: от начала send PCM через RustyOpus encode, два SRTP media
  направления echo, jitter playout и RustyOpus decode до выдачи PCM в Python.
  Echo отражает encoded RTP без второго encode/decode. Это не акустическая
  mouth-to-ear задержка и не сетевой RTT, делённый на два.
* Timestamp сопоставляется по RTP; PLC не включается в успешные RTT samples.
  Сохраняются p50/p95/p99, число посланных, принятых, concealed и unmatched кадров.
* CPU: приращение user+system времени Python-процесса и известных PID helper.
  `cpu_core_percent` считает 100% за одно полностью занятое ядро, может быть >100%.
* RSS: 50 ms samples, пиковая сумма RSS, отдельно parent и helper. Это RSS, не
  PSS; общие страницы могут учитываться несколько раз. Короткие пики между
  samples могут быть пропущены. CPU/RSS включают warmup, RTT samples — нет.
* Сбой/перезапуск worker помечает случай ошибкой вместо публикации смешанных чисел.
  Ошибка любого случая даёт ненулевой exit code.
* Добровольные переключения контекста Linux helper измеряются по счётчику
  процесса. Метрика показывает работу цикла ожидания между кадрами.

Встроенные impairment сценарии воздействуют на RTP echo path после SRTP receive.
Для **настоящих kernel network loss/jitter**, включая ICE/DTLS, имеется netem
wrapper в одноразовом network namespace (root и iproute2 нужны только при запуске):

```bash
sudo .venv/bin/python benchmarks/netem.py --loss 5 --delay-ms 40 --jitter-ms 20 -- \
  .venv/bin/python benchmarks/run.py --scenario clean --mode both \
  --helper "$PWD/target/release/tgcalls-sandbox" --output results/netem.json
```

Изменяется только `lo` нового namespace. Host qdisc не меняется. Для сравнения
используйте release build, фиксированное число calls и один kernel/CPU governor.
Harness не устанавливает governor, не меняет настройки хоста, не требует
Telegram-аккаунта. Скрипты не выполняются при импорте, в тестах или в CI.

## Приватная группа Telegram

`benchmarks/telegram_solo.py` создаёт и закрывает звонок в группе владельца
с одним участником. Стенд использует локальную копию существующей сессии
Telethon и отправляет синтезированное аудио. Укажите отрицательный dialog ID
обычной группы и задайте `TG_API_ID` и `TG_API_HASH` в окружении.

```bash
pip install 'Telethon>=1.40,<2' 'ntgcalls==3.0.7' 'psutil>=6,<8'
python benchmarks/telegram_solo.py --isolated --session "$TG_SESSION_PATH" \
  --existing-group "$TG_SOLO_DIALOG_ID" --helper "$TGCALLS_SANDBOX_BIN" \
  --repetitions 3 --seconds 12 --output results/telegram-solo.json
```

`--isolated` запускает каждое измерение в отдельном процессе и новом звонке,
чтобы загрузка предыдущего backend не влияла на RSS. `setup_ms` включает запрос
на вход, `transport_setup_ms` отсчитывается после ответа Telegram. Стенд
проверяет членство и SSRC. С одним участником измеряются подключение и отправка
аудио. Для замера задержки Telegram и проверки декодирования нужен второй клиент.
