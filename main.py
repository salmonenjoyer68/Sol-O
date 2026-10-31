"""Compatibility entry point for ``uvicorn main:app``.

The application itself lives in ``app.main``; this wrapper keeps the documented
root-level command working without moving Phase 1 code out of ``app/``.
"""

from app.main import app

__all__ = ["app"]
