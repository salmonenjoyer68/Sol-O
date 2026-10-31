"""In-memory event bus and SSE fan-out for live run events (Phase 6).

Every orchestration event flows through :func:`emit`: it is stamped with the
current run id, persisted to ``run_events`` and delivered to any HTTP streams
subscribed through :func:`subscribe`. Publishing is thread-safe because the Sun
runs in a worker thread while SSE readers live on the event loop.
"""

from __future__ import annotations

import asyncio
import logging
import threading
from collections import deque
from collections.abc import Iterator
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from app import logger as store
from app.reliability import redact

logger = logging.getLogger("sol_o.events")

# How many recent events a reconnecting stream may replay (Last-Event-ID).
HISTORY_SIZE = 2000

_run_id: ContextVar[str | None] = ContextVar("sol_o_run_id", default=None)


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


def _clean(value: Any) -> Any:
    """Recursively redact strings so secrets never reach the browser or SQLite."""
    if isinstance(value, str):
        return redact(value)
    if isinstance(value, dict):
        return {str(key): _clean(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_clean(item) for item in value]
    return value


@dataclass
class Subscription:
    """One connected SSE client: an asyncio queue fed from any thread."""

    id: int
    loop: asyncio.AbstractEventLoop
    queue: asyncio.Queue = field(default_factory=asyncio.Queue)

    def deliver(self, event: dict[str, Any]) -> None:
        if self.loop.is_closed():
            return

        def put() -> None:
            self.queue.put_nowait(event)

        try:
            self.loop.call_soon_threadsafe(put)
        except RuntimeError:  # loop closed between the check and the call
            return


class EventBus:
    """Append-only in-memory event log with per-run sequence numbers."""

    def __init__(self, history_size: int = HISTORY_SIZE) -> None:
        self._lock = threading.Lock()
        self._seq: dict[str, int] = {}
        self._history: deque[dict[str, Any]] = deque(maxlen=history_size)
        self._subscribers: dict[int, Subscription] = {}
        self._next_subscription = 0
        self._next_event = 0

    def publish(
        self, run_id: str, event: str, data: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """Stamp, persist, buffer and fan out one event; returns the stored record."""
        with self._lock:
            seq = self._seq.get(run_id, 0) + 1
            self._seq[run_id] = seq
            self._next_event += 1
            record = {
                "id": self._next_event,
                "run_id": run_id,
                "seq": seq,
                "ts": now_iso(),
                "event": event,
                "data": _clean(data or {}),
            }
            self._history.append(record)
            subscribers = list(self._subscribers.values())

        try:
            store.save_run_event(
                run_id=run_id,
                seq=seq,
                ts=record["ts"],
                event=event,
                data=record["data"],
            )
        except Exception:  # telemetry must never take a run down
            logger.exception("could not persist run event run_id=%s event=%s", run_id, event)

        for subscription in subscribers:
            subscription.deliver(record)
        return record

    def subscribe(
        self, loop: asyncio.AbstractEventLoop, last_event_id: int = 0
    ) -> Subscription:
        """Register a client and hand it the buffered backlog after ``last_event_id``."""
        with self._lock:
            self._next_subscription += 1
            subscription = Subscription(id=self._next_subscription, loop=loop)
            backlog = [item for item in self._history if item["id"] > last_event_id]
            self._subscribers[subscription.id] = subscription
        for record in backlog:
            subscription.queue.put_nowait(record)
        return subscription

    def unsubscribe(self, subscription_id: int) -> None:
        with self._lock:
            self._subscribers.pop(subscription_id, None)

    def replay(self, run_id: str) -> list[dict[str, Any]]:
        """Buffered events for one run (unused by the API, handy for tests)."""
        with self._lock:
            return [dict(item) for item in self._history if item["run_id"] == run_id]

    def reset(self) -> None:
        """Drop all counters and subscribers (tests)."""
        with self._lock:
            self._seq.clear()
            self._history.clear()
            self._subscribers.clear()
            self._next_subscription = 0
            self._next_event = 0


bus = EventBus()


@contextmanager
def run_context(run_id: str) -> Iterator[None]:
    """Bind events emitted in this thread to ``run_id`` for the run's duration."""
    token = _run_id.set(run_id)
    try:
        yield
    finally:
        _run_id.reset(token)


def current_run_id() -> str | None:
    return _run_id.get()


def emit(event: str, **data: Any) -> dict[str, Any] | None:
    """Publish through the active run context; a no-op outside a run."""
    run_id = _run_id.get()
    if run_id is None:
        return None
    return bus.publish(run_id, event, data)


def publish_run(run_id: str, event: str, data: dict[str, Any] | None = None) -> dict[str, Any]:
    """Publish for an explicit run id (approval decisions happen outside a run)."""
    return bus.publish(run_id, event, data)
