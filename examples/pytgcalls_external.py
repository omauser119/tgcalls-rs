"""Explicit PyTGCalls group audio integration using ExternalMedia.AUDIO."""
import asyncio
import os
from pathlib import Path

from telethon import TelegramClient
from pytgcalls import PyTgCalls
from pytgcalls.types import MediaStream, ExternalMedia, Device
from pytgcalls.types.raw import AudioParameters
from tgcalls_rs import attach_backend


async def main():
    client = TelegramClient(os.environ.get("TG_SESSION", "tgcalls-rs"),
                            int(os.environ["API_ID"]), os.environ["API_HASH"])
    app = PyTgCalls(client)
    backend = attach_backend(app, hardened=os.environ.get("TGCALLS_HARDENED") == "1",
                             helper_path=os.environ.get("TGCALLS_SANDBOX_BIN"))
    chat = int(os.environ["TG_CHAT"])
    try:
        await app.start()
        await app.play(chat, MediaStream(ExternalMedia.AUDIO,
                                        audio_parameters=AudioParameters(48000, 2),
                                        video_flags=MediaStream.Flags.IGNORE))
        with Path(os.environ["PCM_FILE"]).open("rb") as source:
            deadline = asyncio.get_running_loop().time()
            while pcm := source.read(3840):
                await app.send_frame(chat, Device.MICROPHONE, pcm.ljust(3840, b"\0"))
                deadline += .02
                await asyncio.sleep(max(0, deadline - asyncio.get_running_loop().time()))
        await app.leave_call(chat)
    finally:
        await backend.close()
        await client.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
