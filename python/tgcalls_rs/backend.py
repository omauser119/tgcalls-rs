"""An explicit audio/external-PCM subset of NTgCalls' async interface.

Importing this package does not import or monkeypatch ntgcalls. PyTGCalls still
uses that package for its public enum/data types when attach_backend is used.
"""
from __future__ import annotations

import asyncio
import inspect
import time
from types import SimpleNamespace

from .client import Call


class RustBackend:
    def __init__(self, *, hardened: bool = False, helper_path=None, bind_ip=None):
        self.options = dict(hardened=hardened, helper_path=helper_path, bind_ip=bind_ip)
        self._calls: dict[int, Call] = {}
        self._callbacks: dict[str, object] = {}
        self._start: dict[int, float] = {}
        self._muted: set[int] = set()
        self._receivers: dict[int, asyncio.Task] = {}
        self._external_speaker: set[int] = set()

    async def create_call(self, chat_id: int) -> str:
        if chat_id in self._calls:
            raise RuntimeError("call already exists")
        if len(self._calls) >= 64:
            raise RuntimeError("call capacity reached")
        call = Call(**self.options)
        self._calls[chat_id] = call
        try:
            return await call.create_call()
        except BaseException:
            self._calls.pop(chat_id, None)
            await call.close()
            raise

    async def create_p2p_call(self, *args, **kwargs):
        raise NotImplementedError("Telegram personal calls and encrypted P2P signaling are not implemented")

    async def connect(self, chat_id: int, params: str, is_presentation: bool = False) -> None:
        if is_presentation:
            raise NotImplementedError("screen sharing is not implemented")
        await self._get(chat_id).connect(params)
        self._start[chat_id] = time.monotonic()
        await self._emit("connection_change", chat_id, self._connection_info("CONNECTED"))
        await self._emit("request_participants", chat_id, None)
        if chat_id in self._external_speaker:
            self._receivers[chat_id] = asyncio.create_task(self._receive_frames(chat_id),
                                                         name=f"tgcalls-frames-{chat_id}")

    async def set_stream_sources(self, chat_id, mode, description) -> None:
        self._get(chat_id)
        if description.camera is not None or description.screen is not None:
            raise NotImplementedError("this backend currently supports audio only")
        mic = description.microphone
        if mic is not None:
            if getattr(mic.media_source, "name", "") != "EXTERNAL":
                raise NotImplementedError("use MediaStream(ExternalMedia.AUDIO); feed decoded PCM using send_frame")
            if mic.sample_rate != 48_000 or mic.channel_count != 2:
                raise ValueError("external PCM must be 48 kHz stereo")
        speaker = description.speaker
        if speaker is not None:
            if getattr(speaker.media_source, "name", "") != "EXTERNAL":
                raise NotImplementedError("only external PCM audio output is implemented")
            if speaker.sample_rate != 48_000 or speaker.channel_count != 2:
                raise ValueError("output PCM must be 48 kHz stereo")
            self._external_speaker.add(chat_id)

    async def _receive_frames(self, chat_id):
        from ntgcalls import Frame, FrameData, StreamDevice, StreamMode
        try:
            while chat_id in self._calls:
                try:
                    frame = await self._get(chat_id).receive(.25)
                except TimeoutError:
                    continue
                # FrameData's capture time stays unset: RTP timestamp is not a Unix timestamp.
                await self._emit("frames", chat_id, StreamMode.PLAYBACK, StreamDevice.SPEAKER,
                                 [Frame(frame.ssrc, frame.pcm, FrameData())])
        except asyncio.CancelledError:
            raise
        except Exception:
            await self._emit("connection_change", chat_id, self._connection_info("FAILED"))
            raise

    async def send_external_frame(self, chat_id, device, data: bytes, frame_data=None) -> int:
        kind = getattr(device, "name", str(device)).upper()
        call = self._get(chat_id)
        if kind in {"CAMERA", "SCREEN"}:
            send_i420 = getattr(call, "send_i420", None)
            if send_i420 is None:
                raise NotImplementedError("this call has no external I420 transport")
            if frame_data is None:
                raise ValueError("external video requires frame dimensions")
            return await send_i420(data, frame_data.width, frame_data.height)
        if kind != "MICROPHONE":
            raise NotImplementedError("only microphone PCM frames are supported")
        if chat_id in self._muted:
            data = bytes(len(data))
        return await call.send_pcm(data)

    async def receive(self, chat_id: int, timeout: float = 1):
        return await self._get(chat_id).receive(timeout)

    async def update_audio_ssrc_mappings(self, chat_id: int, mappings) -> None:
        sources = [getattr(item, "ssrc", getattr(item, "source", 0)) & 0xFFFFFFFF for item in mappings]
        await self._get(chat_id).set_sources([s for s in sources if s])

    async def get_connection_mode(self, chat_id):
        self._get(chat_id)
        from ntgcalls import ConnectionMode
        return ConnectionMode.RTC

    async def get_call_type(self, chat_id):
        self._get(chat_id)
        from ntgcalls import CallType
        return CallType.GROUP

    async def get_state(self, chat_id):
        self._get(chat_id)
        from ntgcalls import MediaState
        return MediaState(chat_id in self._muted, False, True, False, True)

    async def calls(self):
        from ntgcalls import StreamStatus, CallInfo
        return {chat_id: CallInfo(StreamStatus.ACTIVE, StreamStatus.ACTIVE)
                for chat_id in self._calls}

    async def mute(self, chat_id):
        self._get(chat_id)
        self._muted.add(chat_id)
        return True

    async def unmute(self, chat_id):
        self._get(chat_id)
        self._muted.discard(chat_id)
        return True

    async def time(self, chat_id, mode=None):
        self._get(chat_id)
        return int((time.monotonic() - self._start.get(chat_id, time.monotonic())) * 1000)

    async def stop(self, chat_id):
        call = self._get(chat_id)
        receiver = self._receivers.pop(chat_id, None)
        if receiver:
            receiver.cancel()
            await asyncio.gather(receiver, return_exceptions=True)
        self._calls.pop(chat_id)
        self._start.pop(chat_id, None)
        self._muted.discard(chat_id)
        self._external_speaker.discard(chat_id)
        await call.close()

    async def close(self):
        for chat_id in list(self._calls):
            await self.stop(chat_id)

    async def stats(self, chat_id):
        return await self._get(chat_id).stats()

    def _get(self, chat_id):
        if chat_id not in self._calls:
            try:
                from ntgcalls import ConnectionNotFound
            except ImportError:
                raise KeyError(chat_id) from None
            raise ConnectionNotFound()
        return self._calls[chat_id]

    def _connection_info(self, state):
        try:
            from ntgcalls import ConnectionInfo, ConnectionState, ConnectionKind
            return ConnectionInfo(getattr(ConnectionState, state), ConnectionKind.NORMAL)
        except ImportError:
            return SimpleNamespace(state=state)

    async def _emit(self, name, *args):
        callback = self._callbacks.get(name)
        if callback:
            result = callback(*args)
            if inspect.isawaitable(result):
                await result

    # Registration is explicit so misspelled or unsupported backend methods fail visibly.
    def on_connection_change(self, callback): self._callbacks["connection_change"] = callback
    def on_request_participants(self, callback): self._callbacks["request_participants"] = callback
    def on_stream_end(self, callback): self._callbacks["stream_end"] = callback
    def on_frames(self, callback): self._callbacks["frames"] = callback
    def on_upgrade(self, callback): self._callbacks["upgrade"] = callback
    def on_update_emojis(self, callback): self._callbacks["update_emojis"] = callback
    def on_signaling_data(self, callback): self._callbacks["signaling_data"] = callback
    def on_request_broadcast_part(self, callback): self._callbacks["request_broadcast_part"] = callback
    def on_request_broadcast_timestamp(self, callback): self._callbacks["request_broadcast_timestamp"] = callback
    def on_outbound_block(self, callback): self._callbacks["outbound_block"] = callback
    def on_subchain_request(self, callback): self._callbacks["subchain_request"] = callback


def attach_backend(pytgcalls, **options) -> RustBackend:
    """Install before PyTgCalls.start(). Only group audio + ExternalMedia.AUDIO is supported."""
    if getattr(pytgcalls, "_is_running", False):
        raise RuntimeError("attach the Rust backend before starting PyTGCalls")
    backend = RustBackend(**options)
    pytgcalls._binding = backend
    return backend
