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
        token in message
        for token in ("response_format", "json_object", "json_schema", "schema", "unsupported")
    )


def call_llm(messages: list[dict[str, str]], response_schema: dict | None = None) -> str:
    """Send chat messages and return the raw assistant content.

    When ``response_schema`` (a JSON Schema) is given, ask the provider to
    enforce it, then fall back to plain JSON mode and finally to no
    ``response_format`` if the provider rejects the richer options.
    """
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

    try:
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
                    "provider rejected response_format type=%s; retrying with a fallback",
                    (response_format or {}).get("type", "none"),
                )

        if completion is None:  # pragma: no cover - the loop always breaks or raises
            raise LLMError("no completion returned")
    except APITimeoutError as exc:
        raise LLMTimeoutError(f"LLM call exceeded {LLM_TIMEOUT_SECONDS:.0f}s") from exc
    except APIError as exc:
        raise LLMError(f"{type(exc).__name__}: {exc}") from exc

    content = completion.choices[0].message.content if completion.choices else None
    if not content or not content.strip():
        raise LLMError("LLM returned an empty response")
    return content
