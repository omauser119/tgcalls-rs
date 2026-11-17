from __future__ import annotations

import asyncio
import json
import secrets
from dataclasses import dataclass
from pathlib import Path

from ._native import Engine, PCM_BYTES


@dataclass(frozen=True)
class AudioFrame:
    ssrc: int
    sequence: int
    timestamp: int
    concealed: bool
    target_delay_ms: int
    pcm: bytes


class Call:
    """One connection. A hardened worker restart changes generation and requires rejoin.

    Cancellation of an asyncio call cannot cancel a native operation already running
    in a thread; every native network wait has its own bounded timeout.
    """

    def __init__(self, *, hardened: bool = False, helper_path: str | Path | None = None,
                 bind_ip: str | None = None):
        self.hardened = hardened
        self._native = Engine(hardened, Path(helper_path) if helper_path else None, bind_ip)
        self._lifecycle = asyncio.Lock()
        self._send_lock = asyncio.Lock()
        self._receive_lock = asyncio.Lock()
        self._closed = False

    async def start_loopback(self, *, loss_percent: float = 0, jitter_ms: int = 0,
                             delay_ms: int = 0, seed: int = 1) -> None:
        async with self._lifecycle:
            self._check_open()
            await asyncio.to_thread(self._native.start_loopback, loss_percent, jitter_ms, delay_ms, seed)

    async def create_call(self, *, ssrc: int | None = None) -> str:
        async with self._lifecycle:
            self._check_open()
            return await asyncio.to_thread(self._native.create_call, ssrc or secrets.randbelow(2**31 - 1) + 1)

    async def join_payload(self) -> str:
        self._check_open()
        return await asyncio.to_thread(self._native.join_payload)

    async def connect(self, telegram_params: str) -> None:
        self._check_open()
        if len(telegram_params.encode()) > 65_536:
            raise ValueError("Telegram transport JSON exceeds 64 KiB")
        await asyncio.to_thread(self._native.connect, telegram_params)

    async def set_sources(self, ssrcs: list[int]) -> None:
        self._check_open()
        if len(ssrcs) > 64:
            raise ValueError("at most 64 remote sources per connection")
        await asyncio.to_thread(self._native.set_sources, list(dict.fromkeys(s & 0xFFFFFFFF for s in ssrcs)))

    async def send_pcm(self, pcm: bytes) -> int:
        self._check_open()
        if not isinstance(pcm, bytes) or len(pcm) != PCM_BYTES:
            raise ValueError(f"expected {PCM_BYTES} bytes of 20 ms stereo s16le PCM")
        async with self._send_lock:
            return await asyncio.to_thread(self._native.send_pcm, pcm)

    async def receive(self, timeout: float = 1) -> AudioFrame:
        self._check_open()
        if not 0 < timeout <= 60:
            raise ValueError("timeout must be in (0, 60] seconds")
        async with self._receive_lock:
            result = await asyncio.to_thread(self._native.receive, max(1, int(timeout * 1000)))
            return AudioFrame(*result)

    async def stats(self) -> dict:
        self._check_open()
        return json.loads(await asyncio.to_thread(self._native.stats_json))

    async def close(self) -> None:
        async with self._lifecycle:
            if not self._closed:
                self._closed = True
                await asyncio.to_thread(self._native.close)

    def _check_open(self) -> None:
        if self._closed:
            raise RuntimeError("call is closed")

    async def __aenter__(self) -> Call:
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.close()
