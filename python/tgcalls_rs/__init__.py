"""Async Rust audio backend. PCM format: stereo s16le, 48 kHz, 20 ms frames."""
from importlib import import_module
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .client import AudioFrame, Call
    from .backend import RustBackend, attach_backend

__all__ = ["AudioFrame", "Call", "RustBackend", "attach_backend"]
__version__ = "0.1.0"


def __getattr__(name):
    if name in {"AudioFrame", "Call"}:
        module = ".client"
    elif name in {"RustBackend", "attach_backend"}:
        module = ".backend"
    else:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    value = getattr(import_module(module, __name__), name)
    globals()[name] = value
    return value


def __dir__():
    return sorted(set(globals()) | set(__all__))
