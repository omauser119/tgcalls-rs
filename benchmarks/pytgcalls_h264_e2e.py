"""Local PyTGCalls I420 → Rust H.264 → SRTP → H.264 benchmark.

With --crypto, both media peers use the pinned official Telegram tde2e frame
crypto and a locally confirmed join block. No Telegram account or server is used.
"""
from __future__ import annotations

import argparse
import asyncio
import ctypes as c
from collections import deque
from importlib.metadata import version
import json
from pathlib import Path
import resource
import statistics
import sys
import time

from pytgcalls import PyTgCalls
from pytgcalls.types import Device, Frame
from telethon import TelegramClient
from telethon.sessions import MemorySession

from tgcalls_rs import attach_backend


MAGIC = b"TGC\x01"
ADDRESSES = ("127.0.0.1:44001", "127.0.0.1:44002")
CHAT = -100123


def library(path, functions):
    lib = c.CDLL(str(path.resolve()))
    for name, result, args in functions:
        symbol = getattr(lib, name)
        symbol.restype = result
        symbol.argtypes = args
    return lib


def peak_rss_kib():
    if sys.platform.startswith("linux"):
        for line in Path("/proc/self/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1])
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss // (1024 if sys.platform == "darwin" else 1)


class Worker:
    def __init__(self, path, wasm):
        self.lib = library(path, [
            ("tgcalls_client_new", c.c_uint64, [c.c_uint8, c.c_char_p, c.c_size_t]),
            ("tgcalls_client_request", c.c_uint64, [c.c_uint64, c.c_char_p, c.c_size_t]),
            ("tgcalls_client_free", None, [c.c_uint64]),
            ("tgcalls_client_generation", c.c_uint64, [c.c_uint64]),
            ("tgcalls_output_data", c.c_void_p, [c.c_uint64]),
            ("tgcalls_output_len", c.c_size_t, [c.c_uint64]),
            ("tgcalls_output_free", None, [c.c_uint64]),
            ("tgcalls_last_error", c.c_char_p, []),
        ])
        self.handle = self.lib.tgcalls_client_new(int(wasm is not None), wasm, len(wasm or b""))
        if not self.handle:
            raise RuntimeError(self.lib.tgcalls_last_error().decode())

    def request(self, data):
        if isinstance(data, dict):
            data = json.dumps(data, separators=(",", ":")).encode()
        output = self.lib.tgcalls_client_request(self.handle, data, len(data))
        if not output:
            raise RuntimeError(self.lib.tgcalls_last_error().decode())
        try:
            address = self.lib.tgcalls_output_data(output)
            size = self.lib.tgcalls_output_len(output)
            if not address or not 0 < size <= 32 * 1024 * 1024:
                raise RuntimeError("invalid worker output span")
            response = json.loads(c.string_at(address, size))
        finally:
            self.lib.tgcalls_output_free(output)
        if not response["ok"]:
            raise RuntimeError(response["error"])
        return response

    def close(self):
        self.lib.tgcalls_client_free(self.handle)


class TelegramCrypto:
    def __init__(self, path):
        self.lib = library(path, [
            ("tgcalls_bench_crypto_new", c.c_void_p, []),
            ("tgcalls_bench_crypto_free", None, [c.c_void_p]),
            ("tgcalls_bench_crypto_frame", c.c_size_t, [c.c_void_p, c.c_int, c.c_bool, c.c_size_t,
                                                        c.c_char_p, c.c_size_t, c.c_void_p, c.c_size_t]),
        ])
        self.handle = self.lib.tgcalls_bench_crypto_new()
        if not self.handle:
            raise RuntimeError("tde2e failed to create the local authenticated conference")
        self.output = c.create_string_buffer(1024 * 1024)

    def apply(self, side, encrypt, prefix, data):
        size = self.lib.tgcalls_bench_crypto_frame(self.handle, side, encrypt, prefix,
                                                 data, len(data), self.output, len(self.output))
        return self.output.raw[:size] if size else b""

    def close(self):
        self.lib.tgcalls_bench_crypto_free(self.handle)


class LoopbackMedia:
    def __init__(self, args):
        self.width, self.height = args.width, args.height
        self.workers = []
        self.crypto = None
        self.pending = deque()
        self.received = []
        self.stats = {"wire_datagrams": 0, "wire_bytes": 0, "encrypted": 0, "decrypted": 0}
        self.reject_tampered = False
        self.reject_replay = False
        self.sent_at = None
        try:
            wasm = args.wasm.read_bytes() if args.wasm else None
            for _ in range(2):
                self.workers.append(Worker(args.library, wasm))
            self.crypto = TelegramCrypto(args.crypto) if args.crypto else None
            for side, address in enumerate(ADDRESSES):
                self.submit(side, {"op": "new", "local": address, "ssrc": side + 101,
                                   "conference": bool(self.crypto), "video": {
                                       "width": args.width, "height": args.height,
                                       "fps": args.fps, "bitrate": 350_000}})
        except BaseException:
            self.shutdown()
            raise

    def submit(self, side, body):
        response = self.workers[side].request(body)
        for packet in response.get("datagrams", []):
            if packet["peer"] != ADDRESSES[1 - side]:
                raise RuntimeError("unexpected network destination in local session")
            self.stats["wire_datagrams"] += 1
            self.stats["wire_bytes"] += len(packet["bytes"])
            self.pending.append((1 - side, {"op": "packet", "peer": ADDRESSES[side], "bytes": packet["bytes"]}))
        for frame in response.get("conference_frames", []):
            data = bytes(frame["bytes"])
            encrypt = frame["encrypt"]
            prefix = frame["clear_bytes"]
            if not encrypt and not self.reject_tampered:
                changed = data[:-1] + bytes([data[-1] ^ 0x80])
                if self.crypto.apply(side, False, 0, changed):
                    raise RuntimeError("tde2e accepted an altered frame")
                self.reject_tampered = True
            transformed = b""
            for _ in range(4 if encrypt else 1):
                transformed = self.crypto.apply(side, encrypt, prefix, data)
                if not encrypt or b"\0\0\1" not in transformed[max(0, prefix - 2):]:
                    break
            if not transformed:
                raise RuntimeError("tde2e rejected the expected frame")
            if not encrypt and not self.reject_replay:
                if self.crypto.apply(side, False, 0, data):
                    raise RuntimeError("tde2e accepted a replayed frame")
                self.reject_replay = True
            self.stats["encrypted" if encrypt else "decrypted"] += 1
            self.pending.append((side, MAGIC + b"\x05" + frame["id"].to_bytes(4, "little") + transformed))
        for frame in response.get("video", []):
            if side != 1 or frame["width"] != self.width or frame["height"] != self.height:
                raise RuntimeError("decoded frame dimensions or recipient mismatch")
            if len(frame["i420"]) != self.width * self.height * 3 // 2:
                raise RuntimeError("decoded I420 size mismatch")
            self.received.append((time.perf_counter() - self.sent_at) * 1000)
        return response["payload"]

    def pump(self):
        for _ in range(1024):
            if not self.pending:
                return
            self.submit(*self.pending.popleft())
        raise RuntimeError("local packet budget exceeded")

    async def connect(self):
        start = time.perf_counter()
        offer = self.submit(0, {"op": "offer"})
        answer = self.submit(1, {"op": "answer", "sdp": offer})
        self.submit(0, {"op": "accept_answer", "sdp": answer})
        while time.perf_counter() - start < 12:
            states = [self.submit(side, {"op": "tick"})["connection_state"] for side in range(2)]
            self.pump()
            if states == ["connected", "connected"]:
                return (time.perf_counter() - start) * 1000
            await asyncio.sleep(.005)
        raise TimeoutError("local ICE/DTLS session did not connect")

    async def send_i420(self, frame, width, height):
        if (width, height) != (self.width, self.height):
            raise ValueError("unexpected I420 input dimensions")
        self.sent_at = time.perf_counter()
        self.submit(0, MAGIC + b"\x04" + frame)
        self.pump()
        return len(frame)

    async def close(self):
        self.shutdown()

    def shutdown(self):
        if self.crypto:
            self.crypto.close()
        for worker in self.workers:
            worker.close()


async def run(args):
    app = PyTgCalls(TelegramClient(MemorySession(), 1, "0" * 32))
    backend = attach_backend(app)
    # Only the MTProto readiness gate is simulated. Both media peers, the
    # public send_frame entry point and the Rust adapter are exercised.
    from types import SimpleNamespace
    app._app = SimpleNamespace(is_connected=True)
    app._is_running = True
    media = LoopbackMedia(args)
    backend._calls[CHAT] = media
    try:
        connect_ms = await media.connect()
        frame_info = Frame.Info(width=args.width, height=args.height)
        n = args.width * args.height
        frames = [bytes(16 + (x + (x // args.width) * 3 + phase) % 220 for x in range(n))
                  + bytes([128]) * (n // 2) for phase in range(8)]
        for index in range(args.warmup):
            await app.send_frame(CHAT, Device.CAMERA, frames[index % len(frames)], frame_info)
        media.received.clear()
        before = media.stats.copy()
        wall, cpu = time.perf_counter(), time.process_time()
        for index in range(args.frames):
            await app.send_frame(CHAT, Device.CAMERA, frames[index % len(frames)], frame_info)
        elapsed, cpu = time.perf_counter() - wall, time.process_time() - cpu
        if len(media.received) != args.frames:
            raise RuntimeError(f"only {len(media.received)} of {args.frames} frames decoded")
        if media.crypto and not (media.reject_tampered and media.reject_replay):
            raise RuntimeError("E2E authentication checks did not run")
        return dict(scenario="local I420 / H264 / SRTP / I420", pytgcalls=version("py-tgcalls"),
                    mode="wasmi" if args.wasm else "native", frame_crypto="TDLib tde2e" if args.crypto else None,
                    width=args.width, height=args.height, configured_fps=args.fps, frames=args.frames,
                    connect_ms=connect_ms, wall_seconds=elapsed, cpu_seconds=cpu,
                    fps=args.frames / elapsed,
                    median_frame_ms=statistics.median(media.received),
                    p95_frame_ms=statistics.quantiles(media.received, n=20, method="inclusive")[18],
                    rss_kib=peak_rss_kib(),
                    rejected_tampered_frame=media.reject_tampered, rejected_replay=media.reject_replay,
                    **{key: value - before[key] for key, value in media.stats.items()})
    finally:
        await backend.stop(CHAT)
        app.executor.shutdown()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", required=True, type=Path)
    parser.add_argument("--wasm", type=Path)
    parser.add_argument("--crypto", type=Path)
    parser.add_argument("--frames", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--width", type=int, default=160)
    parser.add_argument("--height", type=int, default=96)
    parser.add_argument("--fps", type=int, default=10)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not (2 <= args.frames <= 600 and 1 <= args.warmup <= 15 and 16 <= args.width <= 1280
            and 16 <= args.height <= 720 and 1 <= args.fps <= 60
            and args.width % 16 == args.height % 16 == 0):
        parser.error("invalid frame count, resolution or frame rate")
    result = json.dumps(asyncio.run(run(args)), indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(result)
    sys.stdout.write(result)


if __name__ == "__main__":
    main()
