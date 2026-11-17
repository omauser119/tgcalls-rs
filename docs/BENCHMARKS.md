# Методика (бенчмарки не запускались)

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
* CPU: приращение user+system времени Python-процесса и всех его descendants.
  `cpu_core_percent` считает 100% за одно полностью занятое ядро, может быть >100%.
* RSS: 50 ms samples, пиковая сумма дерева, отдельно parent и helper. Это RSS, не
  PSS; общие страницы могут учитываться несколько раз. Короткие пики между
  samples могут быть пропущены. CPU/RSS включают warmup, RTT samples — нет.
* Сбой/перезапуск worker помечает случай ошибкой вместо публикации смешанных чисел.
  Ошибка любого случая даёт ненулевой exit code.

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
