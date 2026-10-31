"""Provider-agnostic chat completions wrapper.

Reads BASE_URL, API_KEY and MODEL from the environment (.env is loaded by
``app.config``) so the same code works with Groq, OpenRouter, Gemini, Ollama and
any other OpenAI-compatible endpoint. Phase 5 adds timeouts, retries with
exponential backoff and redacted error messages.
"""

from __future__ import annotations

import logging
import os

from openai import (
    APIConnectionError,
    APIError,
    APIStatusError,
    APITimeoutError,
    OpenAI,
)

from app.config import settings
from app.reliability import call_with_retries, redact

logger = logging.getLogger("sol_o.llm")


class LLMError(RuntimeError):
    """The provider failed or returned nothing usable."""

    def __init__(self, message: str, *, attempts: int = 1, status_code: int | None = None) -> None:
        message = redact(message)
        super().__init__(message)
        self.attempts = attempts
        self.status_code = status_code


class LLMTimeoutError(LLMError):
    """The provider did not answer within LLM_TIMEOUT_SECONDS."""


def _settings() -> tuple[str, str, str]:
    base_url = os.getenv("BASE_URL", "").strip()
    api_key = os.getenv("API_KEY", "").strip()
    model = os.getenv("MODEL", "").strip()
    missing = [
        name
        for name, value in (("BASE_URL", base_url), ("API_KEY", api_key), ("MODEL", model))
        if not value
    ]
    if missing:
        raise LLMError(f"missing environment variable(s): {', '.join(missing)}")
    return base_url, api_key, model


def _rejects_response_format(exc: APIStatusError) -> bool:
    """Detect a provider 4xx caused specifically by response_format support."""
    message = str(getattr(exc, "message", "") or exc).lower()
    return exc.status_code in (400, 422) and any(
        token in message
        for token in ("response_format", "json_object", "json_schema", "schema", "unsupported")
    )


def _is_retryable(exc: BaseException) -> bool:
    """Timeouts, connection errors, HTTP 429 and 5xx are transient; 4xx are not."""
    if isinstance(exc, (APITimeoutError, APIConnectionError)):
        return True
    if isinstance(exc, APIStatusError):
        return exc.status_code == 429 or exc.status_code >= 500
    return False


def _describe(exc: APIError) -> str:
    status = getattr(exc, "status_code", None)
    body = str(getattr(exc, "message", "") or exc)
    prefix = f"HTTP {status}: " if status else ""
    return f"{prefix}{body}"[:500]


def _request(
    client: OpenAI,
    messages: list[dict[str, str]],
    response_schema: dict | None,
    model: str,
    timeout: float,
) -> str:
    """One provider request, falling back through weaker response formats."""
    request: dict = {"model": model, "messages": messages, "timeout": timeout}

    formats: list[dict | None] = []
    if response_schema is not None:
        formats.append(
            {
                "type": "json_schema",
                "json_schema": {
                    "name": "agent_output",
                    "strict": True,
                    "schema": response_schema,
                },
            }
        )
    formats.extend([{"type": "json_object"}, None])

    completion = None
    for index, response_format in enumerate(formats):
        current = dict(request)
        if response_format is not None:
            current["response_format"] = response_format
        try:
            completion = client.chat.completions.create(**current)
            break
        except APIStatusError as exc:
            if index == len(formats) - 1 or not _rejects_response_format(exc):
                raise
            logger.info(
                "provider rejected response_format type=%s; trying a fallback",
                (response_format or {}).get("type", "none"),
            )

    if completion is None:  # pragma: no cover - the loop always breaks or raises
        raise LLMError("no completion returned")

    content = completion.choices[0].message.content if completion.choices else None
    if not content or not content.strip():
        raise LLMError("LLM returned an empty response")
    return content


def call_llm(
    messages: list[dict[str, str]],
    response_schema: dict | None = None,
    timeout: float | None = None,
) -> str:
    """Send chat messages and return the raw assistant content.

    Transient failures (timeout, connection, 429, 5xx) are retried up to
    ``LLM_MAX_RETRIES`` with 1s/2s/4s backoff. 4xx errors fail immediately.
    """
    base_url, api_key, model = _settings()
    call_timeout = float(timeout) if timeout is not None else settings.llm_timeout_seconds
    call_timeout = max(0.001, call_timeout)
    client = OpenAI(base_url=base_url, api_key=api_key, timeout=call_timeout, max_retries=0)

    attempts = {"count": 0}

    def attempt() -> str:
        attempts["count"] += 1
        return _request(client, messages, response_schema, model, call_timeout)

    def on_retry(retry_number: int, delay: float, exc: BaseException) -> None:
        logger.warning(
            "LLM retry %d/%d in %.1fs after %s: %s",
            retry_number,
            settings.llm_max_retries,
            delay,
            type(exc).__name__,
            redact(str(exc))[:300],
        )

    try:
        return call_with_retries(
            attempt,
            max_retries=settings.llm_max_retries,
            retry_if=_is_retryable,
            on_retry=on_retry,
        )
    except APITimeoutError as exc:
        raise LLMTimeoutError(
            f"LLM call exceeded {call_timeout:.1f}s after {attempts['count']} attempt(s)",
            attempts=attempts["count"],
        ) from exc
    except APIStatusError as exc:
        raise LLMError(
            _describe(exc), attempts=attempts["count"], status_code=exc.status_code
        ) from exc
    except APIError as exc:
        raise LLMError(_describe(exc), attempts=attempts["count"]) from exc
