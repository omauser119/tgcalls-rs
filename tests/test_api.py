import asyncio
import os

import pytest

from tgcalls_rs import Call


@pytest.mark.parametrize("hardened", [False, True])
async def test_python_pcm_roundtrip(hardened):
    helper = os.environ.get("TGCALLS_SANDBOX_BIN")
    if hardened and not helper:
        pytest.skip("set TGCALLS_SANDBOX_BIN to test hardened mode")
    async with Call(hardened=hardened, helper_path=helper) as call:
        await call.start_loopback()

        async def send():
            for _ in range(8):
                await call.send_pcm(bytes(3840))
                await asyncio.sleep(.02)

        sending = asyncio.create_task(send())
        try:
            frame = await call.receive(3)
            assert len(frame.pcm) == 3840
            assert isinstance(frame.pcm, bytes)
            assert not frame.concealed
            stats = await call.stats()
            assert stats["hardened"] is hardened
            assert bool(stats["worker_pid"]) is hardened
        finally:
            await sending


async def test_input_validation_and_closed_state():
    call = Call()
    with pytest.raises(ValueError):
        await call.send_pcm(b"short")
    with pytest.raises(ValueError):
        await call.start_loopback(loss_percent=float("nan"))
    await call.close()
    with pytest.raises(RuntimeError, match="closed"):
        await call.start_loopback()


async def test_hardened_requires_explicit_helper(monkeypatch):
    monkeypatch.delenv("TGCALLS_SANDBOX_BIN", raising=False)
    async with Call(hardened=True) as call:
        with pytest.raises(RuntimeError, match="helper"):
            await call.start_loopback()


async def test_receive_timeout_is_python_timeout():
    async with Call() as call:
        await call.start_loopback()
        with pytest.raises(TimeoutError):
            await call.receive(.05)
