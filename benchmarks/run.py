"""Opt-in Linux benchmark harness. Import, --help and --list never start calls.

RTT measures encode -> RTP -> ICE/DTLS/SRTP echo -> jitter playout -> decode.
This is a digital media RTT, not acoustic mouth-to-ear latency. Warmup is excluded
from RTT samples. CPU/RSS cover the full media run, including warmup, and include
the Python process and all helper descendants.
"""
from __future__ import annotations

import argparse
import asyncio
import importlib.metadata
import json
import math
import os
import platform
import statistics
import sys
import time
from array import array
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass(frozen=True)
class Scenario:
    name: str
    loss_percent: float = 0
    jitter_ms: int = 0
    delay_ms: int = 0


SCENARIOS = [Scenario("clean"), Scenario("loss-1", 1), Scenario("loss-5", 5),
             Scenario("jitter-20", jitter_ms=20), Scenario("loss-jitter", 5, 40, 20)]


def percentile(values, p):
    if not values:
        return None
    values = sorted(values)
    location = (len(values) - 1) * p / 100
    low, high = math.floor(location), math.ceil(location)
    return values[low] + (values[high] - values[low]) * (location - low)


class ProcessTreeSampler:
    def __init__(self):
        import psutil
        self.psutil = psutil
        self.parent = psutil.Process()
        self.previous = {}
        self.cpu_seconds = 0.0
        self.peak_rss = 0
        self.peak_parent_rss = 0
        self.peak_worker_rss = 0
        self.samples = 0

    def sample(self, *, baseline=False):
        processes = [self.parent, *self.parent.children(recursive=True)]
        parent_rss = worker_rss = 0
        for process in processes:
            try:
                key = process.pid, process.create_time()
                cpu = process.cpu_times()
                total_cpu = cpu.user + cpu.system
                old_cpu = self.previous.get(key, total_cpu if baseline else 0)
                self.cpu_seconds += max(0, total_cpu - old_cpu)
                self.previous[key] = total_cpu
                rss = process.memory_info().rss
                if process.pid == self.parent.pid:
                    parent_rss += rss
                else:
                    worker_rss += rss
            except (self.psutil.NoSuchProcess, self.psutil.AccessDenied):
                continue
        self.peak_parent_rss = max(self.peak_parent_rss, parent_rss)
        self.peak_worker_rss = max(self.peak_worker_rss, worker_rss)
        self.peak_rss = max(self.peak_rss, parent_rss + worker_rss)
        self.samples += 1

    async def run(self, finished):
        self.sample(baseline=True)
        while not finished.is_set():
            await asyncio.sleep(.05)
            self.sample()


def tone():
    samples = array("h", (int(4000 * math.sin(2 * math.pi * 440 * n / 48000))
                          for n in range(960) for _ in range(2)))
    if sys.byteorder != "little":
        samples.byteswap()
    return samples.tobytes()


async def media_run(call, duration, warmup):
    pending = {}
    rtts = []
    sent = received = concealed = 0
    pcm = tone()
    loop = asyncio.get_running_loop()
    begin = loop.time()
    finish = begin + warmup + duration
    done = asyncio.Event()

    async def producer():
        nonlocal sent
        index = 0
        deadline = begin
        try:
            while loop.time() < finish:
                timestamp = (index * 960) & 0xFFFFFFFF
                measured = loop.time() >= begin + warmup
                start_ns = time.perf_counter_ns()
                if measured:
                    pending[timestamp] = start_ns
                actual = await call.send_pcm(pcm)
                if actual != timestamp:
                    raise RuntimeError("RTP clock reset during benchmark; result would be invalid")
                if measured:
                    sent += 1
                index += 1
                deadline += .02
                await asyncio.sleep(max(0, deadline - loop.time()))
        finally:
            done.set()

    async def consumer():
        nonlocal received, concealed
        while not done.is_set() or loop.time() < finish + .5:
            try:
                frame = await call.receive(.25)
            except TimeoutError:
                continue
            if frame.concealed:
                if loop.time() >= begin + warmup:
                    concealed += 1
                continue
            started = pending.pop(frame.timestamp, None)
            if started is not None:
                rtts.append((time.perf_counter_ns() - started) / 1e6)
                received += 1

    await asyncio.gather(producer(), consumer())
    return dict(sent=sent, received=received, concealed=concealed,
                unmatched=len(pending), rtt_ms=rtts)


async def one_case(mode, scenario, args):
    from tgcalls_rs import Call
    calls = []
    setup = []
    before_generation = []
    try:
        for _ in range(args.calls):
            call = Call(hardened=mode == "hardened", helper_path=args.helper)
            calls.append(call)
            begin = time.perf_counter_ns()
            await call.start_loopback(loss_percent=scenario.loss_percent,
                                      jitter_ms=scenario.jitter_ms,
                                      delay_ms=scenario.delay_ms, seed=args.seed)
            setup.append((time.perf_counter_ns() - begin) / 1e6)
            before_generation.append((await call.stats())["generation"])
        sampler = ProcessTreeSampler()
        finished = asyncio.Event()
        sampling = asyncio.create_task(sampler.run(finished))
        start = time.perf_counter()
        try:
            streams = await asyncio.gather(*(media_run(call, args.seconds, args.warmup) for call in calls))
        finally:
            finished.set()
            await sampling
        wall = time.perf_counter() - start
        stats = [await call.stats() for call in calls]
        if any(s["generation"] != g for s, g in zip(stats, before_generation)):
            raise RuntimeError("sandbox restarted: refusing to report misleading performance numbers")
        rtts = [rtt for stream in streams for rtt in stream["rtt_ms"]]
        return dict(mode=mode, scenario=asdict(scenario), concurrent_calls=args.calls,
                    connection_setup_ms=setup,
                    rtt_ms=dict(count=len(rtts), median=statistics.median(rtts) if rtts else None,
                                p95=percentile(rtts, 95), p99=percentile(rtts, 99)),
                    frames={name: sum(s[name] for s in streams) for name in ("sent", "received", "concealed", "unmatched")},
                    resources=dict(cpu_seconds=sampler.cpu_seconds, wall_seconds=wall,
                                   cpu_core_percent=100 * sampler.cpu_seconds / wall,
                                   peak_tree_rss_bytes=sampler.peak_rss,
                                   peak_parent_rss_bytes=sampler.peak_parent_rss,
                                   peak_worker_rss_bytes=sampler.peak_worker_rss,
                                   sample_interval_ms=50, samples=sampler.samples),
                    engine_stats=stats)
    finally:
        await asyncio.gather(*(call.close() for call in calls), return_exceptions=True)


async def run(args):
    modes = ["standard", "hardened"] if args.mode == "both" else [args.mode]
    scenarios = [s for s in SCENARIOS if not args.scenario or s.name in args.scenario]
    results = []
    failed = False
    for repetition in range(args.repetitions):
        # Alternate order to reduce systematic warm-cache / thermal bias.
        for mode in modes if repetition % 2 == 0 else list(reversed(modes)):
            for scenario in scenarios:
                try:
                    result = await one_case(mode, scenario, args)
                    result["repetition"] = repetition
                except Exception as error:
                    failed = True
                    result = dict(mode=mode, scenario=asdict(scenario), repetition=repetition, error=str(error))
                results.append(result)
    artifact = dict(schema_version=1, python=platform.python_version(), kernel=platform.release(),
                    machine=platform.machine(), cpu_count=os.cpu_count(),
                    versions={name: importlib.metadata.version(name) for name in ("tgcalls-rs", "psutil")},
                    duration_seconds=args.seconds, warmup_seconds=args.warmup,
                    impairment_location="RTP echo media path; use netem.py for kernel network impairment",
                    external_network_impairment=os.environ.get("TGCALLS_NETEM"), results=results)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(artifact, indent=2) + "\n")
    print(args.output)
    return 1 if failed else 0


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--mode", choices=["standard", "hardened", "both"], default="both")
    result.add_argument("--helper", type=Path, default=os.environ.get("TGCALLS_SANDBOX_BIN"))
    result.add_argument("--seconds", type=float, default=30)
    result.add_argument("--warmup", type=float, default=2)
    result.add_argument("--calls", type=int, default=1)
    result.add_argument("--repetitions", type=int, default=3)
    result.add_argument("--seed", type=int, default=1)
    result.add_argument("--scenario", choices=[s.name for s in SCENARIOS], action="append")
    result.add_argument("--output", type=Path, default=Path("results/benchmark.json"))
    result.add_argument("--list", action="store_true", help="print the test matrix without starting calls")
    return result


if __name__ == "__main__":
    arguments = parser().parse_args()
    if arguments.list:
        print(json.dumps([asdict(s) for s in SCENARIOS], indent=2))
    elif not (0 < arguments.seconds <= 3600 and 0 <= arguments.warmup <= 60
              and 1 <= arguments.calls <= 64 and 1 <= arguments.repetitions <= 100):
        raise SystemExit("invalid duration, warmup, call count or repetition count")
    else:
        raise SystemExit(asyncio.run(run(arguments)))
