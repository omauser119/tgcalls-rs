from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from tgcalls_rs import RustBackend, attach_backend

n = pytest.importorskip("ntgcalls")


async def test_adapter_uses_actual_ntgcalls_types():
    backend = RustBackend()
    fake = SimpleNamespace(set_sources=AsyncMock(), send_pcm=AsyncMock(return_value=960), close=AsyncMock())
    backend._calls[-100] = fake
    await backend.set_stream_sources(-100, n.StreamMode.CAPTURE, n.MediaDescription(
        microphone=n.AudioDescription(n.MediaSource.EXTERNAL, 48000, 2, "", False), speaker=None, camera=None, screen=None))
    assert await backend.get_connection_mode(-100) == n.ConnectionMode.RTC
    assert await backend.get_call_type(-100) == n.CallType.GROUP
    state = await backend.get_state(-100)
    assert isinstance(state, n.MediaState)
    assert not state.muted
    await backend.mute(-100)
    assert (await backend.get_state(-100)).muted
    await backend.send_external_frame(-100, n.StreamDevice.MICROPHONE, bytes([1]) * 3840)
    fake.send_pcm.assert_awaited_once_with(bytes(3840))
    await backend.update_audio_ssrc_mappings(-100, [n.SsrcMapping(1, -1)])
    fake.set_sources.assert_awaited_once_with([0xFFFFFFFF])
    assert isinstance(backend._connection_info("CONNECTED"), n.ConnectionInfo)
    assert (await backend.calls())[-100].capture == n.StreamStatus.ACTIVE
    await backend.stop(-100)
    fake.close.assert_awaited_once()


async def test_unsupported_media_fails_explicitly():
    backend = RustBackend()
    backend._calls[1] = object()
    description = n.MediaDescription(n.AudioDescription(n.MediaSource.FILE, 48000, 2, "file", False), None, None, None)
    with pytest.raises(NotImplementedError, match="ExternalMedia"):
        await backend.set_stream_sources(1, n.StreamMode.CAPTURE, description)
    with pytest.raises(NotImplementedError):
        await backend.send_external_frame(1, n.StreamDevice.CAMERA, b"")


def test_attach_and_real_pytgcalls_external_media():
    from telethon import TelegramClient
    from telethon.sessions import MemorySession
    from pytgcalls import PyTgCalls
    from pytgcalls.types import MediaStream, ExternalMedia
    from pytgcalls.types.raw import AudioParameters
    client = TelegramClient(MemorySession(), 12345, "0" * 32)
    app = PyTgCalls(client)
    backend = attach_backend(app)
    assert app._binding is backend
    stream = MediaStream(ExternalMedia.AUDIO, audio_parameters=AudioParameters(48000, 2),
                         video_flags=MediaStream.Flags.IGNORE)
    assert stream.microphone is not None
    assert stream.camera is None
    app._is_running = True
    with pytest.raises(RuntimeError, match="before"):
        attach_backend(app)


async def test_received_pcm_uses_real_frame_callback_types():
    import asyncio
    from tgcalls_rs import AudioFrame
    backend = RustBackend()
    frame = AudioFrame(7, 0, 960, False, 40, bytes(3840))
    fake = SimpleNamespace(receive=AsyncMock(side_effect=[frame, asyncio.CancelledError()]))
    backend._calls[-100] = fake
    delivered = []
    backend.on_frames(lambda *args: delivered.append(args))
    with pytest.raises(asyncio.CancelledError):
        await backend._receive_frames(-100)
    chat_id, mode, device, frames = delivered[0]
    assert chat_id == -100
    assert mode == n.StreamMode.PLAYBACK
    assert device == n.StreamDevice.SPEAKER
    assert isinstance(frames[0], n.Frame)
    assert frames[0].data == bytes(3840)
