"""Reliability helpers: retries with backoff, run deadline, redaction (Phase 5)."""

from __future__ import annotations

import logging
import os
import re
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import TypeVar

logger = logging.getLogger("sol_o.reliability")

T = TypeVar("T")

BACKOFF_BASE_SECONDS = 1.0
MAX_RAW_CHARS = 2000
MAX_MESSAGE_CHARS = 1000

_SECRET_ENV_VARS = ("API_KEY", "TAVILY_API_KEY", "TELEGRAM_BOT_TOKEN")
_SECRET_PATTERNS = (
    re.compile(r"sk-[A-Za-z0-9_\-]{8,}"),
    re.compile(r"(bot\d+:)[A-Za-z0-9_\-]{10,}"),
    re.compile(r"(?i)(bearer\s+)[A-Za-z0-9._\-]{8,}"),
)


def redact(text: str) -> str:
    """Remove known secrets and token-shaped strings from logs, errors and payloads."""
    if not text:
        return text
    result = text
    for name in _SECRET_ENV_VARS:
        value = os.getenv(name, "").strip()
        if len(value) >= 8:  # short placeholders such as "ollama" are not secrets
            result = result.replace(value, "***")
    for pattern in _SECRET_PATTERNS:
        result = pattern.sub(lambda match: match.group(1) + "***" if match.groups() else "***", result)
    return result


def truncate(text: str, limit: int = MAX_RAW_CHARS) -> str:
    """Hard cap stored raw output without splitting the record."""
    if text is None:
        return ""
    return text if len(text) <= limit else text[:limit]


def retry_delay(retry_number: int) -> float:
    """1s, 2s, 4s, ... for retry numbers starting at 1."""
    return BACKOFF_BASE_SECONDS * (2 ** max(0, retry_number - 1))


def call_with_retries(
    fn: Callable[[], T],
    *,
    max_retries: int,
    retry_if: Callable[[BaseException], bool],
    on_retry: Callable[[int, float, BaseException], None] | None = None,
    sleep: Callable[[float], None] | None = None,
) -> T:
    """Call ``fn``; retry only when ``retry_if`` says so, with exponential backoff."""
    sleeper = time.sleep if sleep is None else sleep
    retry_number = 0
    while True:
        try:
            return fn()
        except BaseException as exc:  # re-raised unless explicitly retryable
            if retry_number >= max_retries or not retry_if(exc):
                raise
            retry_number += 1
            delay = retry_delay(retry_number)
            if on_retry is not None:
                on_retry(retry_number, delay, exc)
            sleeper(delay)


class RunTimeout(RuntimeError):
    """The whole-run time budget was exhausted."""


@dataclass
class Deadline:
    """Wall-clock budget for one orchestration run."""

    started: float
    seconds: float

    def remaining(self) -> float:
        return max(0.0, self.seconds - (time.perf_counter() - self.started))

    def expired(self) -> bool:
        return self.remaining() <= 0.0


def run_deadline(seconds: float, started: float | None = None) -> Deadline:
    return Deadline(started=time.perf_counter() if started is None else started, seconds=seconds)


class RedactFilter(logging.Filter):
    """Redact secrets from every log record before handlers format it."""

    def filter(self, record: logging.LogRecord) -> bool:
        try:
            message = record.getMessage()
        except Exception:  # noqa: BLE001
            # malformed args: leave the record untouched
            return True
        record.msg = redact(message)
        record.args = ()
        return True


def install_redaction_filter() -> None:
    """Attach the redaction filter to the root handlers created by basicConfig."""
    root = logging.getLogger()
    for handler in root.handlers:
        if not any(isinstance(item, RedactFilter) for item in handler.filters):
            handler.addFilter(RedactFilter())
