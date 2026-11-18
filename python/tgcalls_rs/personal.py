"""Telethon adapter for individual Telegram calls using the Rust media worker."""
from __future__ import annotations

import asyncio
import ipaddress
import json
import secrets
import socket

from ._native import PersonalWorker


class TelegramPersonalCall:
    """One call. Pass a connected Telethon user client with updates enabled.

    Construct from an incoming PhoneCallRequested update to acknowledge it and
    call accept() or hangup(busy=True) explicitly. For outgoing calls use dial().
    PCM and I420 are provided by the caller of this adapter.
    """

    def __init__(self, client, *, video=None, bind_ip=None, outgoing=True):
        self.client = client
        self.video = video
        self.bind_ip = bind_ip
        self.outgoing = outgoing
        self.state = "idle"
        self.remote_media = {}
        self.verification_hash = None
        self.error = None
        self._worker = PersonalWorker()
        self._peer = None
        self._requested = None
        self._started = None
        self._socket = None
        self._network_task = None
        self._tasks = set()
        self._updates = asyncio.Queue(64)
        self.audio = asyncio.Queue(16)
        self.video_frames = asyncio.Queue(4)
        self._early_signaling = []
        self._update_handler = self._on_update
        self._lock = asyncio.Lock()

    @staticmethod
    def protocol():
        from telethon import types
        return types.PhoneCallProtocol(min_layer=92, max_layer=92, library_versions=["9.0.0", "8.0.0"],
                                       udp_p2p=True, udp_reflector=False)

    async def _run_worker(self, op, **fields):
        data = json.dumps(dict(op=op, **fields), separators=(",", ":")).encode()
        async with self._lock:
            result = json.loads(await asyncio.to_thread(self._worker.request, data))
            if not result["ok"]:
                raise RuntimeError(result["error"])
            if self._socket is not None:
                loop = asyncio.get_running_loop()
                for datagram in result.get("datagrams", []):
                    host, port = datagram["peer"].rsplit(":", 1)
                    address = str(ipaddress.ip_address(host.strip("[]")))
                    await loop.sock_sendto(self._socket, bytes(datagram["bytes"]), (address, int(port)))
            for frames, queue in (("audio", self.audio), ("video", self.video_frames)):
                for frame in result.get(frames, []):
                    if queue.full():
                        queue.get_nowait()
                    queue.put_nowait(frame)
            return result

    async def _dh(self, commitment=None):
        from telethon import functions, types
        config = await self.client(functions.messages.GetDhConfigRequest(version=0, random_length=256))
        if not isinstance(config, types.messages.DhConfig):
            raise RuntimeError("Telegram did not return DH parameters")
        result = await self._run_worker("init_dh", g=config.g, prime=list(config.p), random=list(config.random),
                                        commitment=list(commitment) if commitment is not None else None)
        return bytes(result["payload"])

    def _listen(self):
        from telethon import events
        self.client.add_event_handler(self._update_handler, events.Raw())

    def _remember(self, call):
        from telethon import types
        if self._peer is None:
            self._peer = types.InputPhoneCall(call.id, call.access_hash)

    def _task(self, coroutine):
        task = asyncio.create_task(coroutine)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    async def dial(self, user):
        from telethon import functions, types, utils
        if self.state != "idle" or not self.outgoing:
            raise RuntimeError("already started")
        target = utils.get_input_user(await self.client.get_input_entity(user))
        self._listen()
        try:
            response = await self.client(functions.phone.RequestCallRequest(
                user_id=target, random_id=secrets.randbits(31), g_a_hash=await self._dh(),
                protocol=self.protocol(), video=self.video is not None,
            ))
            if isinstance(response.phone_call, types.PhoneCallDiscarded):
                raise RuntimeError("call discarded")
            self._remember(response.phone_call)
            self.state = "ringing"
            self._task(self._watch(response.phone_call))
        except BaseException:
            await self.hangup()
            raise

    async def receive_request(self, requested):
        from telethon import functions, types
        if self.state != "idle" or self.outgoing or not isinstance(requested, types.PhoneCallRequested):
            raise RuntimeError("expected a fresh phoneCallRequested")
        if requested.participant_id != (await self.client.get_me()).id:
            raise ValueError("call request belongs to another account")
        self._remember(requested)
        self._requested = requested
        self.state = "ringing"
        self._listen()
        try:
            await self.client(functions.phone.ReceivedCallRequest(peer=self._peer))
        except BaseException:
            await self._close()
            raise
        self._task(self._watch())

    async def accept(self):
        from telethon import functions
        if self.state != "ringing" or self._requested is None:
            raise RuntimeError("no pending incoming call")
        self.state = "accepting"
        try:
            gb = await self._dh(self._requested.g_a_hash)
            response = await self.client(functions.phone.AcceptCallRequest(
                peer=self._peer, g_b=gb, protocol=self.protocol(),
            ))
            self._updates.put_nowait(response.phone_call)
        except BaseException:
            await self.hangup()
            raise

    async def _on_update(self, update):
        from telethon import types
        if self._peer is None:
            return
        if isinstance(update, types.UpdatePhoneCall):
            value = update.phone_call
            if value.id != self._peer.id:
                return
        elif isinstance(update, types.UpdatePhoneCallSignalingData) and update.phone_call_id == self._peer.id:
            value = update.data
        else:
            return
        if self._updates.full():
            self.error = RuntimeError("call update queue exhausted")
            await self.hangup()
        else:
            self._updates.put_nowait(value)

    async def _watch(self, initial=None):
        from telethon import functions, types
        try:
            update = initial
            while True:
                if update is None:
                    update = await self._updates.get()
                if isinstance(update, types.PhoneCallDiscarded):
                    await self._close()
                    return
                if isinstance(update, types.PhoneCallAccepted) and self.outgoing and self.state == "ringing":
                    self.state = "confirming"
                    result = (await self._run_worker("complete_dh", remote=list(update.g_b)))["payload"]
                    self.verification_hash = bytes(result["verification_hash"])
                    reply = await self.client(functions.phone.ConfirmCallRequest(
                        peer=self._peer, g_a=bytes(result["public"]), key_fingerprint=result["fingerprint"],
                        protocol=self.protocol(),
                    ))
                    update = reply.phone_call
                    continue
                if isinstance(update, types.PhoneCall) and self.state in {"accepting", "confirming"}:
                    if not self.outgoing:
                        result = await self._run_worker("complete_dh", remote=list(update.g_a_or_b),
                                                        fingerprint=update.key_fingerprint)
                        self.verification_hash = bytes(result["payload"]["verification_hash"])
                    await self._connect(update)
                elif isinstance(update, bytes):
                    if self._socket is not None:
                        await self._run_worker("signaling", packet=list(update))
                    elif len(self._early_signaling) < 16 and len(update) <= 16_384:
                        self._early_signaling.append(update)
                update = None
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.error = error
            await self.hangup(disconnected=True)

    async def _connect(self, call):
        from telethon import types
        if not set(call.protocol.library_versions).intersection(self.protocol().library_versions):
            raise RuntimeError("no supported Telegram call signaling version")
        if call.custom_parameters and json.loads(call.custom_parameters.data).get("network_use_mtproto"):
            raise RuntimeError("Telegram MTProto media transport is unavailable")
        servers = []
        for server in call.connections:
            if not isinstance(server, types.PhoneConnectionWebrtc):
                continue
            if server.ip:
                common = dict(host=server.ip, port=server.port, username=server.username, password=server.password)
                if server.stun:
                    servers.append(dict(common, turn=False))
                if server.turn:
                    servers.append(dict(common, turn=True))
        ip = self.bind_ip
        if ip is None:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
                probe.connect((servers[0]["host"], servers[0]["port"]) if servers else ("149.154.167.50", 443))
                ip = probe.getsockname()[0]
        self._socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._socket.setblocking(False)
        self._socket.bind((ip, 0))
        address = self._socket.getsockname()
        await self._run_worker("new", local=f"{address[0]}:{address[1]}", ssrc=secrets.randbelow(2**31 - 1) + 1,
                               video=self.video, ice_servers=servers[:16], relay_only=not call.p2p_allowed)
        await self._run_worker("start_personal", outgoing=self.outgoing)
        for packet in self._early_signaling:
            await self._run_worker("signaling", packet=list(packet))
        self._early_signaling.clear()
        self.state = "connecting"
        self._network_task = self._task(self._network())
        self._task(self._tick())

    async def _network(self):
        try:
            loop = asyncio.get_running_loop()
            while self._socket is not None:
                packet, (address, port) = await loop.sock_recvfrom(self._socket, 65535)
                await self._run_worker("packet", peer=f"{address}:{port}", bytes=list(packet))
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.error = error
            await self.hangup(disconnected=True)

    async def _tick(self):
        from telethon import functions
        try:
            while self._socket is not None:
                tick = (await self._run_worker("tick"))["payload"]
                self.remote_media = tick["remote_media"]
                if tick["connection_state"].lower() == "connected":
                    self.state = "connected"
                    self._started = self._started or asyncio.get_running_loop().time()
                elif self.state == "connected":
                    self.state = "reconnecting"
                elif tick["connection_state"].lower() in {"failed", "closed"}:
                    raise ConnectionError("media transport failed")
                for packet in (await self._run_worker("poll_signaling")).get("signaling", []):
                    await self.client(functions.phone.SendSignalingDataRequest(peer=self._peer, data=bytes(packet)))
                await asyncio.sleep(.01)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.error = error
            await self.hangup(disconnected=True)

    async def media_state(self, *, muted=False, camera="inactive"):
        await self._run_worker("media_state", state={"muted": muted, "videoState": camera})

    async def change_network(self, bind_ip):
        """Rebind after the host observes a network-interface change."""
        if self._socket is None or self.state not in {"connected", "reconnecting"}:
            raise RuntimeError("no active call to rebind")
        replacement = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        replacement.setblocking(False)
        try:
            replacement.bind((bind_ip, 0))
        except BaseException:
            replacement.close()
            raise
        self._network_task.cancel()
        await asyncio.gather(self._network_task, return_exceptions=True)
        old = self._socket
        self._socket = replacement
        self.state = "reconnecting"
        host, port = replacement.getsockname()
        try:
            await self._run_worker("rebind_personal", local=f"{host}:{port}")
        except BaseException:
            await self.hangup(disconnected=True)
            raise
        finally:
            old.close()
        self._network_task = self._task(self._network())

    async def configure_audio(self, *, capture_rate, capture_channels=1, render_rate=48000,
                              render_channels=2, echo_cancellation=True, noise_suppression=True,
                              automatic_gain=True):
        await self._run_worker(
            "configure_audio", capture_rate=capture_rate, capture_channels=capture_channels,
            render_rate=render_rate, render_channels=render_channels,
            options=dict(echo_cancellation=echo_cancellation, noise_suppression=noise_suppression,
                         automatic_gain=automatic_gain),
        )

    async def capture(self, pcm, delay_ms=0):
        await self._run_worker("capture_audio", pcm=list(pcm), delay_ms=delay_ms)

    async def render_reference(self, pcm):
        await self._run_worker("render_audio", pcm=list(pcm))

    async def send_pcm(self, pcm):
        return (await self._run_worker("send_audio", pcm=list(pcm)))["payload"]

    async def send_i420(self, frame):
        await self._run_worker("send_video", i420=list(frame))

    async def hangup(self, *, busy=False, disconnected=False):
        from telethon import functions, types
        if self.state in {"closed", "closing"}:
            return
        self.state = "closing"
        try:
            if self._peer is not None:
                reason = (types.PhoneCallDiscardReasonBusy() if busy else
                          types.PhoneCallDiscardReasonDisconnect() if disconnected else
                          types.PhoneCallDiscardReasonHangup())
                duration = 0 if self._started is None else int(asyncio.get_running_loop().time() - self._started)
                await self.client(functions.phone.DiscardCallRequest(
                    peer=self._peer, duration=duration, reason=reason,
                    connection_id=0, video=self.video is not None,
                ))
        finally:
            await self._close()

    async def _close(self):
        self.state = "closed"
        self.client.remove_event_handler(self._update_handler)
        running = tuple(task for task in self._tasks if task is not asyncio.current_task())
        for task in running:
            task.cancel()
        await asyncio.gather(*running, return_exceptions=True)
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        self._worker = None
