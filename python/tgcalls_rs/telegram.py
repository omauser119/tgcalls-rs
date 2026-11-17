"""Telethon bridge to actual Telegram group calls (not a loopback replacement)."""
from __future__ import annotations

import asyncio
import json

from .client import Call


class TelegramGroupCall:
    def __init__(self, client, chat, **engine_options):
        self.client = client
        self.chat = chat
        self.audio = Call(**engine_options)
        self._call = None
        self._peer = None
        self._ssrc = None
        self._watchdog = None
        self._generation = 0
        self._closing = False
        self.last_error: Exception | None = None
        self._lock = asyncio.Lock()

    async def join(self, *, muted: bool = False) -> None:
        from telethon import functions, types, utils
        entity = await self.client.get_entity(self.chat)
        if isinstance(entity, types.Channel):
            full = await self.client(functions.channels.GetFullChannelRequest(entity))
        elif isinstance(entity, types.Chat):
            full = await self.client(functions.messages.GetFullChatRequest(entity.id))
        else:
            raise ValueError("target must be a group or supergroup with an active voice chat")
        call = full.full_chat.call
        if call is None:
            raise RuntimeError("start a Telegram voice chat in the target group first")
        self._call = types.InputGroupCall(call.id, call.access_hash)
        self._peer = utils.get_input_peer(await self.client.get_me())
        try:
            payload = await self.audio.create_call()
            await self._join_transport(payload, muted)
        except BaseException:
            await self.close()
            raise
        self._watchdog = asyncio.create_task(self._maintain(), name="tgcalls-telegram-maintenance")

    async def _join_transport(self, payload: str, muted: bool = False) -> None:
        from telethon import functions, types
        async with self._lock:
            self._ssrc = json.loads(payload)["ssrc"]
            result = await self.client(functions.phone.JoinGroupCallRequest(
                call=self._call, join_as=self._peer, params=types.DataJSON(payload),
                muted=muted, video_stopped=True,
            ))
            response = next((u.params.data for u in result.updates
                             if isinstance(u, types.UpdateGroupCallConnection) and not u.presentation), None)
            if response is None:
                raise RuntimeError("Telegram did not return group-call transport parameters")
            await self.audio.connect(response)
            self._generation = (await self.audio.stats())["generation"]
            await self.refresh_participants()

    async def refresh_participants(self) -> None:
        from telethon import functions
        participants = await self.client(functions.phone.GetGroupParticipantsRequest(
            call=self._call, ids=[], sources=[], offset="", limit=100,
        ))
        my_ssrc = self._ssrc & 0xFFFFFFFF if self._ssrc is not None else None
        sources = [p.source & 0xFFFFFFFF for p in participants.participants
                   if p.source and p.source & 0xFFFFFFFF != my_ssrc and not p.left]
        await self.audio.set_sources(sources)

    async def _maintain(self) -> None:
        from telethon import functions
        try:
            tick = 0
            while not self._closing:
                await asyncio.sleep(1)
                stats = await self.audio.stats()
                if stats["generation"] != self._generation or stats["needs_rejoin"]:
                    await self._join_transport(await self.audio.join_payload())
                # Telegram expects liveness queries; participant SSRCs can change mid-call.
                tick += 1
                if self._ssrc is not None and tick % 5 == 0:
                    await self.client(functions.phone.CheckGroupCallRequest(call=self._call, sources=[self._ssrc]))
                    await self.refresh_participants()
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.last_error = error
            # Surface loss of the Telegram transport; never silently keep sending PCM.
            await self.audio.close()

    async def close(self) -> None:
        if self._closing:
            return
        self._closing = True
        if self._watchdog:
            self._watchdog.cancel()
            await asyncio.gather(self._watchdog, return_exceptions=True)
        try:
            if self._call is not None and self._ssrc is not None:
                from telethon import functions
                await self.client(functions.phone.LeaveGroupCallRequest(call=self._call, source=self._ssrc))
        finally:
            await self.audio.close()

    async def __aenter__(self):
        await self.join()
        return self

    async def __aexit__(self, *_):
        await self.close()
