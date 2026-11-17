"""Join an existing Telegram voice chat and stream a raw PCM file.

No credentials or session files are committed. This script makes a real call only
when explicitly invoked. API_ID, API_HASH, TG_CHAT must be set by the operator.
"""
import argparse
import asyncio
import os
from pathlib import Path

from telethon import TelegramClient
from tgcalls_rs.telegram import TelegramGroupCall


async def main(args):
    chat = os.environ["TG_CHAT"]
    if chat.lstrip("-").isdigit():
        chat = int(chat)
    async with TelegramClient(os.environ.get("TG_SESSION", "tgcalls-rs"),
                              int(os.environ["API_ID"]), os.environ["API_HASH"]) as client:
        async with TelegramGroupCall(client, chat, hardened=args.hardened,
                                     helper_path=args.helper) as call:
            loop = asyncio.get_running_loop()
            deadline = loop.time()
            with args.pcm.open("rb") as source:
                while pcm := source.read(3840):
                    if call.last_error:
                        raise call.last_error
                    await call.audio.send_pcm(pcm.ljust(3840, b"\0"))
                    deadline += .02
                    await asyncio.sleep(max(0, deadline - loop.time()))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pcm", type=Path, help="48 kHz stereo signed 16-bit little-endian PCM")
    parser.add_argument("--hardened", action="store_true")
    parser.add_argument("--helper", type=Path)
    asyncio.run(main(parser.parse_args()))
