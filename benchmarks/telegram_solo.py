"""Measure an upload to a private Telegram group with one member.

Uses an already authorized Telethon session. Never invites participants, reads
microphone input, or persists session data and Telegram identifiers in results.
"""
from __future__ import annotations

import argparse
import asyncio
import importlib.metadata
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from run import ProcessTreeSampler, tone


async def check_solo(client, group, call=None):
    from telethon import functions, types

    if isinstance(group, types.Chat):
        full = await client(functions.messages.GetFullChatRequest(group.id))
        participants = full.full_chat.participants
        solo = isinstance(participants, types.ChatParticipants) and len(participants.participants) == 1
    else:
        full = await client(functions.channels.GetFullChannelRequest(group))
        solo = full.full_chat.participants_count == 1 and group.megagroup and not group.username and not group.usernames
    if not solo or (call is None and full.full_chat.call is not None):
        raise RuntimeError("benchmark requires a private solo group with no existing call")
    if call is not None:
        members = await client(functions.phone.GetGroupParticipantsRequest(
            call=call, ids=[], sources=[], offset="", limit=2,
        ))
        if members.count != 1 or len(members.participants) != 1:
            raise RuntimeError("expected exactly one call participant")
        participant = members.participants[0]
        if not isinstance(participant.peer, types.PeerUser) or participant.peer.user_id != (await client.get_me()).id:
            raise RuntimeError("unexpected call participant")


async def measure(client, group, call, mode, args):
    from telethon import functions, types
    from tgcalls_rs.telegram import check_group_sources

    loop = asyncio.get_running_loop()
    connected = asyncio.Event()
    terminated = asyncio.Event()
    rust = None
    native = None
    source = None
    chat_id = group.id
    if mode == "ntgcalls":
        from ntgcalls import (
            AudioDescription, ConnectionMode, ConnectionState, FrameData,
            MediaDescription, MediaSource, NTgCalls, StreamDevice, StreamMode,
        )
    else:
        from tgcalls_rs import Call
    try:
        start = time.perf_counter()
        if mode == "ntgcalls":
            native = NTgCalls()
            def on_connection(peer, info):
                if peer == chat_id:
                    if info.state == ConnectionState.CONNECTED:
                        loop.call_soon_threadsafe(connected.set)
                    elif info.state in {ConnectionState.FAILED, ConnectionState.CLOSED, ConnectionState.TIMEOUT}:
                        loop.call_soon_threadsafe(terminated.set)

            native.on_connection_change(on_connection)
            payload = await native.create_call(chat_id)
            mic = AudioDescription(MediaSource.EXTERNAL, 48_000, 2, "", False)
            await native.set_stream_sources(chat_id, StreamMode.CAPTURE, MediaDescription(mic, None, None, None))
            metadata = FrameData()

            async def send(pcm):
                await native.send_external_frame(chat_id, StreamDevice.MICROPHONE, pcm, metadata)
        else:
            rust = Call(hardened=mode == "hardened", helper_path=args.helper)
            payload = await rust.create_call()
            send = rust.send_pcm

        source = json.loads(payload)["ssrc"]
        updates = await client(functions.phone.JoinGroupCallRequest(
            call=call, join_as=types.InputPeerSelf(), params=types.DataJSON(payload),
            muted=False, video_stopped=True,
        ))
        params = next((u.params.data for u in updates.updates
                       if isinstance(u, types.UpdateGroupCallConnection) and not u.presentation), None)
        if params is None:
            raise RuntimeError("missing group transport response")
        async def establish_transport():
            if native is None:
                await rust.connect(params)
                return
            await native.connect(chat_id, params, False)
            if await native.get_connection_mode(chat_id) != ConnectionMode.RTC:
                raise RuntimeError("expected Telegram RTC transport")
            await connected.wait()

        connect_start = time.perf_counter()
        try:
            await asyncio.wait_for(establish_transport(), 12)
        except Exception as error:
            # Prohibit connection data or credentials from entering the artifacts.
            raise RuntimeError(f"transport setup: {type(error).__name__}") from None
        setup_ms = (time.perf_counter() - start) * 1000
        transport_setup_ms = (time.perf_counter() - connect_start) * 1000
        await check_solo(client, group, call)
        initial_stats = await rust.stats() if rust is not None else None
        worker_pids = [initial_stats["worker_pid"]] if initial_stats else []
        sampler = ProcessTreeSampler(worker_pids)
        pcm = tone()
        blocks = (pcm,)
        if native is not None:
            # NTgCalls AudioSink reads one 10 ms block per external callback.
            # Schedule both halves so the engines consume identical PCM.
            half = len(pcm) // 2
            blocks = (pcm[:half], pcm[half:])
        step = .02 / len(blocks)
        frames = round(args.seconds * 50)
        count = frames * len(blocks)
        start = time.perf_counter()
        last_check = start
        sampler.sample(baseline=True)
        for index in range(count):
            if terminated.is_set():
                raise RuntimeError("call disconnected")
            await send(blocks[index % len(blocks)])
            if (index + 1) % (3 * len(blocks)) == 0:
                sampler.sample()
            if time.perf_counter() - last_check >= 4:
                live = await check_group_sources(client, call, [source])
                if source not in live:
                    raise RuntimeError("Telegram no longer sees local audio source")
                await check_solo(client, group, call)
                last_check = time.perf_counter()
            await asyncio.sleep(max(0, start + (index + 1) * step - time.perf_counter()))
        elapsed = time.perf_counter() - start
        sampler.sample()
        await check_solo(client, group, call)
        live = await check_group_sources(client, call, [source])
        if source not in live:
            raise RuntimeError("Telegram did not retain local audio source")
        if rust is not None:
            stats = await rust.stats()
            if (stats["media"]["connection_state"].lower() != "connected"
                    or stats["needs_rejoin"] or stats["generation"] != initial_stats["generation"]):
                raise RuntimeError("Rust transport did not stay connected")
            if stats["media"]["sent_frames"] - initial_stats["media"]["sent_frames"] != frames:
                raise RuntimeError("Rust did not send every submitted frame")
        return {
            "mode": mode, "setup_ms": setup_ms, "transport_setup_ms": transport_setup_ms,
            "submitted_frames": frames,
            "submitted_blocks": count, "block_duration_ms": 1000 * step,
            "submitted_pcm_bytes": frames * len(pcm),
            "wall_seconds": elapsed, "cpu_seconds": sampler.cpu_seconds,
            "peak_tree_rss_bytes": sampler.peak_rss, "call_participants": 1,
            "worker_voluntary_switches": sampler.worker_switch_delta(),
        }
    finally:
        try:
            if source is not None:
                await client(functions.phone.LeaveGroupCallRequest(call=call, source=source))
        finally:
            if rust is not None:
                await rust.close()
            if native is not None:
                await asyncio.gather(native.stop(chat_id), return_exceptions=True)


async def run(args):
    from telethon import TelegramClient, errors, functions, types

    with tempfile.TemporaryDirectory(prefix="tgcalls-bench-") as temp:
        session = Path(temp) / "test.session"
        session.touch(mode=0o600)
        # Backup through SQLite so the original remains usable and unchanged.
        with sqlite3.connect(args.session.resolve().as_uri() + "?mode=ro", uri=True) as src:
            with sqlite3.connect(session) as dst:
                src.backup(dst)
        client = TelegramClient(str(session), int(os.environ["TG_API_ID"]), os.environ["TG_API_HASH"],
                                connection_retries=1, request_retries=1, flood_sleep_threshold=0)
        group = call = None
        created_group = False
        reports = []
        artifact = {
            "schema": 4,
            "scenario": "private Telegram group, one participant, synthesized 48 kHz stereo PCM upload",
            "scope": "upload and transport setup only; no remote decode, RTT or general security claim",
            "ntgcalls_baseline": importlib.metadata.version("ntgcalls"),
            "seconds_per_run": args.seconds,
            "transport_deadline_seconds": 12,
            "results": reports,
        }
        try:
            await client.connect()
            if not await client.is_user_authorized():
                raise RuntimeError("provided session is not authorized")
            if args.existing_group is not None:
                group = await client.get_entity(args.existing_group)
                if not isinstance(group, (types.Chat, types.Channel)) or not group.creator:
                    raise RuntimeError("existing benchmark target must be a group you own")
            else:
                async for dialog in client.iter_dialogs(limit=1000):
                    entity = dialog.entity
                    if (
                        not isinstance(entity, (types.Chat, types.Channel))
                        or not entity.creator or entity.call_active
                    ):
                        continue
                    try:
                        await check_solo(client, entity)
                    except RuntimeError:
                        continue
                    group = entity
                    break
            if group is None:
                from telethon.errors import ChannelsTooMuchError
                try:
                    created = await client(functions.channels.CreateChannelRequest(
                        title="Temporary solo audio benchmark", about="", megagroup=True,
                    ))
                except ChannelsTooMuchError:
                    raise RuntimeError("account at Telegram group limit; no owned private solo group available") from None
                group, = (item for item in created.chats if isinstance(item, types.Channel))
                created_group = True
            await check_solo(client, group)
            started = await client(functions.phone.CreateGroupCallRequest(peer=group, title="Solo audio test"))
            found = next(u.call for u in started.updates if isinstance(u, types.UpdateGroupCall))
            call = types.InputGroupCall(found.id, found.access_hash)
            modes = args.mode or ["standard", "hardened", "ntgcalls"]
            for repeat in range(args.repetitions):
                for mode in modes if repeat % 2 == 0 else reversed(modes):
                    try:
                        row = await asyncio.wait_for(measure(client, group, call, mode, args), args.seconds + 45)
                    except errors.FloodWaitError as error:
                        reports.append({"mode": mode, "error": "FloodWaitError", "repetition": repeat})
                        artifact["retry_after_seconds"] = error.seconds
                        print(mode, "Telegram rate limit", flush=True)
                        return 1
                    except Exception as error:
                        row = {"mode": mode, "error": type(error).__name__}
                        if isinstance(error, RuntimeError) and str(error).startswith("transport setup:"):
                            row["stage"] = str(error)
                    reports.append(dict(row, repetition=repeat))
                    print(mode, row.get("stage", row.get("error", "completed")), flush=True)
        except errors.FloodWaitError as error:
            artifact["retry_after_seconds"] = error.seconds
            print("Telegram rate limit", flush=True)
            return 1
        finally:
            try:
                try:
                    if call is not None:
                        await client(functions.phone.DiscardGroupCallRequest(call=call))
                finally:
                    try:
                        if created_group and group is not None:
                            await client(functions.channels.DeleteChannelRequest(group))
                    finally:
                        await client.disconnect()
            except errors.FloodWaitError as error:
                artifact["retry_after_seconds"] = max(artifact.get("retry_after_seconds", 0), error.seconds)
                artifact["cleanup_error"] = type(error).__name__
                raise
            finally:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(artifact, indent=2) + "\n")
    return 1 if any("error" in row for row in reports) else 0


def run_isolated(args):
    modes = args.mode or ["standard", "hardened", "ntgcalls"]
    results = []
    artifact = {}
    exit_status = 0
    with tempfile.TemporaryDirectory(prefix="tgcalls-measurements-") as temp:
        for repeat in range(args.repetitions):
            for mode in modes if repeat % 2 == 0 else reversed(modes):
                path = Path(temp) / f"{repeat}-{mode}.json"
                command = [
                    sys.executable, __file__, "--session", str(args.session),
                    "--helper", str(args.helper), "--existing-group", str(args.existing_group),
                    "--seconds", str(args.seconds), "--repetitions", "1",
                    "--mode", mode, "--output", str(path),
                ]
                if subprocess.run(command, check=False).returncode:
                    exit_status = 1
                if path.is_file():
                    artifact = json.loads(path.read_text())
                    if "retry_after_seconds" in artifact:
                        results.extend(dict(row, repetition=repeat) for row in artifact["results"])
                        break
                    if artifact["results"]:
                        results.extend(dict(row, repetition=repeat) for row in artifact["results"])
                        continue
                results.append({"mode": mode, "repetition": repeat, "error": "measurement process failed"})
            if "retry_after_seconds" in artifact:
                break
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(dict(artifact, results=results, process_isolation=True), indent=2) + "\n")
    return exit_status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", type=Path, required=True)
    parser.add_argument("--existing-group", type=int, help="Telegram dialog ID of a private solo group you own")
    parser.add_argument("--isolated", action="store_true", help="one fresh process and call per measurement")
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--seconds", type=float, default=12)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--mode", choices=["standard", "hardened", "ntgcalls"], action="append")
    parser.add_argument("--output", type=Path, default=Path("results/telegram-solo.json"))
    args = parser.parse_args()
    if not (5 <= args.seconds <= 120 and 1 <= args.repetitions <= 10):
        parser.error("duration must be 5–120 seconds and repetitions 1–10")
    if not args.session.is_file() or not args.helper.is_file():
        parser.error("session and sandbox helper must be existing files")
    if not all(os.environ.get(k) for k in ("TG_API_ID", "TG_API_HASH")):
        parser.error("set TG_API_ID and TG_API_HASH")
    if args.isolated:
        if args.existing_group is None:
            parser.error("--isolated requires --existing-group")
        return run_isolated(args)
    return asyncio.run(run(args))


if __name__ == "__main__":
    raise SystemExit(main())
