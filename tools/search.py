"""Tavily web search used by the Researcher planet (Phase 4 tool).

Plain Python tool: the Sun calls ``search()`` and injects the results into the
researcher's message. The model never calls tools through function calling.
Phase 5 adds a tool timeout and one retry on timeout/5xx.
"""

from __future__ import annotations

import logging
import os

import httpx

from app.config import settings
from app.reliability import call_with_retries, redact

logger = logging.getLogger("sol_o.tools.search")

TAVILY_ENDPOINT = "https://api.tavily.com/search"


class SearchUnavailable(RuntimeError):
    """Search could not run (missing key, network error, bad response)."""

    def __init__(self, message: str) -> None:
        super().__init__(redact(message))


def _is_transient(exc: BaseException) -> bool:
    if isinstance(exc, httpx.TransportError):
        return True
    if isinstance(exc, httpx.HTTPStatusError):
        return exc.response.status_code >= 500
    return False


def search(query: str, max_results: int = 5) -> list[dict[str, str]]:
    """Return ``[{title, url, content}]`` for ``query`` or raise SearchUnavailable."""
    api_key = os.getenv("TAVILY_API_KEY", "").strip()
    if not api_key:
        raise SearchUnavailable("TAVILY_API_KEY is not set")
    cleaned = " ".join(query.split())
    if not cleaned:
        raise SearchUnavailable("empty search query")

    def post_once() -> httpx.Response:
        response = httpx.post(
            TAVILY_ENDPOINT,
            headers={"Authorization": f"Bearer {api_key}"},
            json={
                "query": cleaned[:400],
                "max_results": max(1, min(int(max_results), 10)),
                "search_depth": "basic",
                "include_answer": False,
            },
            timeout=settings.tool_timeout_seconds,
        )
        response.raise_for_status()
        return response

    try:
        response = call_with_retries(
            post_once,
            max_retries=1,
            retry_if=_is_transient,
            on_retry=lambda number, delay, exc: logger.warning(
                "search retry %d in %.1fs after %s", number, delay, type(exc).__name__
            ),
        )
        payload = response.json()
    except (httpx.HTTPError, ValueError) as exc:
        raise SearchUnavailable(f"search request failed: {exc}") from exc

    raw_results = payload.get("results") if isinstance(payload, dict) else None
    if not isinstance(raw_results, list):
        raise SearchUnavailable("unexpected Tavily response shape")

    results: list[dict[str, str]] = []
    for item in raw_results:
        if not isinstance(item, dict):
            continue
        url = str(item.get("url") or "").strip()
        title = str(item.get("title") or "").strip()
        content = " ".join(str(item.get("content") or "").split())
        if not url:
            continue
        results.append({"title": title, "url": url, "content": content})
    logger.info("search query=%r results=%d", cleaned[:80], len(results))
    return results[:max_results]
