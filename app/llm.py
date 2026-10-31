"""Provider-agnostic chat completions wrapper for Phase 1.

Reads BASE_URL, API_KEY and MODEL from the environment (.env is loaded once at
import time) so the same code works with Groq, OpenRouter, Gemini, Ollama and
any other OpenAI-compatible endpoint.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

from dotenv import load_dotenv
from openai import (
    APIError,
    APIStatusError,
    APITimeoutError,
    OpenAI,
)

load_dotenv(Path(__file__).resolve().parent.parent / ".env")

logger = logging.getLogger("sol_o.llm")

LLM_TIMEOUT_SECONDS = 30.0


class LLMError(RuntimeError):
    """The provider failed or returned nothing usable."""


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
        token in message for token in ("response_format", "json_object", "unsupported")
    )


def call_llm(messages: list[dict[str, str]]) -> str:
    """Send chat messages and return the raw assistant content."""
    base_url, api_key, model = _settings()
    client = OpenAI(
        base_url=base_url,
        api_key=api_key,
        timeout=LLM_TIMEOUT_SECONDS,
        max_retries=0,
    )
    request: dict = {
        "model": model,
        "messages": messages,
        "timeout": LLM_TIMEOUT_SECONDS,
    }

    try:
        try:
            completion = client.chat.completions.create(
                response_format={"type": "json_object"}, **request
            )
        except APIStatusError as exc:
            if not _rejects_response_format(exc):
                raise
            logger.info("provider rejected response_format; retrying without it")
            completion = client.chat.completions.create(**request)
    except APITimeoutError as exc:
        raise LLMTimeoutError(
            f"LLM call exceeded {LLM_TIMEOUT_SECONDS:.0f}s"
        ) from exc
    except APIError as exc:
        raise LLMError(f"{type(exc).__name__}: {exc}") from exc

    content = completion.choices[0].message.content if completion.choices else None
    if not content or not content.strip():
        raise LLMError("LLM returned an empty response")
    return content
