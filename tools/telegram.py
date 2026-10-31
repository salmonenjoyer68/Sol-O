"""Telegram notifier used as the last Sun step (Phase 4 tool).

``DRY_RUN=true`` (the default) prints the message instead of sending it, so the
full pipeline is runnable without bot credentials. Phase 5 adds the tool
timeout, one retry on timeout/5xx, and redacted errors.
"""

from __future__ import annotations

import logging
import os

import httpx

from app.config import settings
from app.reliability import call_with_retries, redact

logger = logging.getLogger("sol_o.tools.telegram")

TELEGRAM_LIMIT = 4000  # Bot API hard limit is 4096; stay below it.
TRUTHY = {"1", "true", "yes", "on"}


class TelegramError(RuntimeError):
    """The message could not be delivered."""

    def __init__(self, message: str) -> None:
        super().__init__(redact(message))


def dry_run_enabled() -> bool:
    """Default to true: never send unless explicitly disabled."""
    return os.getenv("DRY_RUN", "true").strip().lower() in TRUTHY


def split_message(text: str, limit: int = TELEGRAM_LIMIT) -> list[str]:
    """Split on paragraph boundaries, then hard-wrap, never exceeding ``limit``."""
    if len(text) <= limit:
        return [text]

    chunks: list[str] = []
    current = ""
    for paragraph in text.split("\n\n"):
        candidate = f"{current}\n\n{paragraph}" if current else paragraph
        if len(candidate) <= limit:
            current = candidate
            continue
        if current:
            chunks.append(current)
            current = ""
        while len(paragraph) > limit:
            chunks.append(paragraph[:limit])
            paragraph = paragraph[limit:]
        current = paragraph
    if current:
        chunks.append(current)
    return chunks


def _is_transient(exc: BaseException) -> bool:
    if isinstance(exc, httpx.TransportError):
        return True
    if isinstance(exc, httpx.HTTPStatusError):
        return exc.response.status_code >= 500
    return False


def send_message(text: str) -> dict[str, object]:
    """Send ``text`` (or print it under DRY_RUN); return delivery metadata."""
    parts = split_message(text)
    if dry_run_enabled():
        print("\n--- DRY_RUN telegram message ---", flush=True)
        for index, part in enumerate(parts, start=1):
            header = f"[part {index}/{len(parts)}] "
            print(f"{header}{part}", flush=True)
        print("--- end telegram message ---", flush=True)
        logger.info("telegram dry_run parts=%d characters=%d", len(parts), len(text))
        return {"dry_run": True, "delivered": False, "parts": len(parts)}

    token = os.getenv("TELEGRAM_BOT_TOKEN", "").strip()
    chat_id = os.getenv("TELEGRAM_CHAT_ID", "").strip()
    if not token or not chat_id:
        raise TelegramError("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be set")

    url = f"https://api.telegram.org/bot{token}/sendMessage"
    for part in parts:

        def send_once(part: str = part) -> None:
            response = httpx.post(
                url,
                json={"chat_id": chat_id, "text": part, "disable_web_page_preview": True},
                timeout=settings.tool_timeout_seconds,
            )
            response.raise_for_status()

        try:
            call_with_retries(
                send_once,
                max_retries=1,
                retry_if=_is_transient,
                on_retry=lambda number, delay, exc: logger.warning(
                    "telegram retry %d in %.1fs after %s", number, delay, type(exc).__name__
                ),
            )
        except httpx.HTTPError as exc:
            raise TelegramError(f"telegram send failed: {exc}") from exc

    logger.info("telegram sent parts=%d characters=%d", len(parts), len(text))
    return {"dry_run": False, "delivered": True, "parts": len(parts)}
