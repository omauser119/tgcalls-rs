"""Async Rust audio backend. PCM format: stereo s16le, 48 kHz, 20 ms frames."""
from .client import AudioFrame, Call
from .backend import RustBackend, attach_backend

__all__ = ["AudioFrame", "Call", "RustBackend", "attach_backend"]
__version__ = "0.1.0"
