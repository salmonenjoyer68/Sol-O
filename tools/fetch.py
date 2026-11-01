"""SSRF-guarded page fetch used by planets through the Sun's tool loop.

Only public http(s) targets are fetched: private, loopback, link-local,
reserved, multicast and unspecified addresses are rejected before any request,
and every redirect hop is re-validated. The body is capped and reduced to its
visible text so a page can be injected into a prompt safely.
"""

from __future__ import annotations

import ipaddress
import logging
import re
import socket
from html.parser import HTMLParser
from typing import Any, ClassVar
from urllib.parse import urljoin, urlsplit

import httpx

from app.config import settings
from app.reliability import call_with_retries, redact

logger = logging.getLogger("sol_o.tools.fetch")

MAX_BYTES = 200 * 1024
MAX_TEXT_CHARS = 6000
MAX_REDIRECTS = 3
ALLOWED_SCHEMES = {"http", "https"}
WHITESPACE_RE = re.compile(r"[ \t\r\f\v]+")


class FetchError(RuntimeError):
    """The URL could not be fetched safely."""

    def __init__(self, message: str) -> None:
        super().__init__(redact(message))


class _TextExtractor(HTMLParser):
    """Collect visible text while skipping script/style subtrees."""

    SKIP: ClassVar[frozenset[str]] = frozenset({"script", "style", "noscript", "template"})

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self._depth = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag in self.SKIP:
            self._depth += 1

    def handle_endtag(self, tag: str) -> None:
        if tag in self.SKIP and self._depth:
            self._depth -= 1

    def handle_data(self, data: str) -> None:
        if not self._depth:
            text = WHITESPACE_RE.sub(" ", data).strip()
            if text:
                self.parts.append(text)


def _reject(reason: str) -> FetchError:
    return FetchError(f"url rejected: {reason}")


def _check_address(host: str, port: int) -> None:
    """Reject any resolved address that is not publicly routable."""
    try:
        addresses = {info[4][0] for info in socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP)}
    except OSError as exc:
        raise FetchError(f"could not resolve host {host}: {exc}") from exc
    for address in addresses:
        try:
            parsed = ipaddress.ip_address(address.split("%", 1)[0])
        except ValueError:
            raise FetchError(f"unparseable address for {host}") from None
        if (
            parsed.is_private
            or parsed.is_loopback
            or parsed.is_link_local
            or parsed.is_reserved
            or parsed.is_multicast
            or parsed.is_unspecified
        ):
            raise _reject("url is not publicly routable")


def _assert_public_url(url: str) -> str:
    """Validate one hop and return its port-qualified host for address checks."""
    parts = urlsplit(url.strip())
    if parts.scheme not in ALLOWED_SCHEMES:
        raise _reject(f"unsupported scheme {parts.scheme!r}")
    if parts.username or parts.password:
        raise _reject("credentials in url")
    host = parts.hostname or ""
    if not host:
        raise _reject("missing host")
    port = parts.port or (443 if parts.scheme == "https" else 80)

    try:
        literal = ipaddress.ip_address(host.split("%", 1)[0])
    except ValueError:
        _check_address(host, port)
    else:
        if (
            literal.is_private
            or literal.is_loopback
            or literal.is_link_local
            or literal.is_reserved
            or literal.is_multicast
            or literal.is_unspecified
        ):
            raise _reject("url is not publicly routable")
    return host


def _extract_text(body: bytes, encoding: str | None) -> str:
    text = body.decode(encoding or "utf-8", errors="replace")
    if "<" not in text:
        return text
    parser = _TextExtractor()
    try:
        parser.feed(text)
        parser.close()
    except (AssertionError, ValueError):  # malformed HTML: fall back to a character strip
        logger.warning("html parse failed; falling back to a character strip")
        return WHITESPACE_RE.sub(" ", re.sub(r"<[^>]+>", " ", text)).strip()
    return " ".join(parser.parts)


def _is_transient(exc: BaseException) -> bool:
    if isinstance(exc, httpx.TransportError):
        return True
    if isinstance(exc, httpx.HTTPStatusError):
        return exc.response.status_code >= 500
    return False


def _fetch_once(url: str) -> dict[str, Any]:
    """Follow up to MAX_REDIRECTS public hops and return capped visible text."""
    current = url
    with httpx.Client(
        follow_redirects=False,
        timeout=settings.tool_timeout_seconds,
        headers={"User-Agent": "Sol-O/1.0 (+tool fetch)"},
    ) as client:
        for _hop in range(MAX_REDIRECTS + 1):
            _assert_public_url(current)
            with client.stream("GET", current) as response:
                if response.is_redirect and response.headers.get("location"):
                    current = urljoin(current, response.headers["location"])
                    continue
                response.raise_for_status()
                chunks: list[bytes] = []
                total = 0
                truncated = False
                for chunk in response.iter_bytes():
                    remaining = MAX_BYTES - total
                    if remaining <= 0:
                        truncated = True
                        break
                    chunks.append(chunk[:remaining])
                    total += min(len(chunk), remaining)
                    if len(chunk) > remaining:
                        truncated = True
                        break
                body = b"".join(chunks)
                text = _extract_text(body, response.encoding)
                if len(text) > MAX_TEXT_CHARS:
                    text = text[:MAX_TEXT_CHARS]
                    truncated = True
                return {
                    "url": str(response.url),
                    "status": response.status_code,
                    "text": text,
                    "truncated": truncated,
                }
    raise FetchError(f"too many redirects (>{MAX_REDIRECTS})")


def fetch_url(url: str) -> dict[str, Any]:
    """Fetch one public page and return ``{url, status, text, truncated}``."""
    cleaned = (url or "").strip()
    if not cleaned:
        raise FetchError("empty url")
    return call_with_retries(
        lambda: _fetch_once(cleaned),
        max_retries=1,
        retry_if=_is_transient,
    )
