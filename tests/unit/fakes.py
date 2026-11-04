"""Shared offline HTTP fakes for the fetch tests in this package."""

from __future__ import annotations

import contextlib
import socket
from collections.abc import Iterator
from typing import Any

import pytest

from tools import fetch


class Page:
    """Fake httpx response served by :func:`install_http`."""

    def __init__(
        self,
        url: str,
        *,
        status_code: int = 200,
        body: bytes = b"",
        headers: dict[str, str] | None = None,
    ) -> None:
        self.url = url
        self.status_code = status_code
        self.body = body
        self.encoding = "utf-8"
        self.headers = headers or {}
        self.is_redirect = status_code in {301, 302, 303, 307, 308}

    def raise_for_status(self) -> None:
        return None

    def iter_bytes(self) -> Iterator[bytes]:
        yield self.body


def install_http(monkeypatch: pytest.MonkeyPatch, pages: dict[str, Page]) -> list[str]:
    """Serve scripted pages and return the list of requested urls."""
    requested: list[str] = []

    class FakeClient:
        def stream(self, method: str, url: str) -> Any:
            requested.append(url)
            return contextlib.nullcontext(pages[url])

    @contextlib.contextmanager
    def fake_client(**kwargs: Any) -> Iterator[FakeClient]:
        yield FakeClient()

    monkeypatch.setattr(fetch.httpx, "Client", fake_client)
    return requested


def forbid_dns(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make hostname resolution fail the test if it happens."""

    def no_dns(*args: Any, **kwargs: Any) -> Any:
        raise AssertionError("resolution must not run for a rejected url")

    monkeypatch.setattr(socket, "getaddrinfo", no_dns)


def forbid_http(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make any HTTP client construction fail the test."""

    class NoRequestClient:
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            pass

        def __enter__(self):
            return self

        def __exit__(self, *exc: object) -> None:
            return None

        def stream(self, *args: Any, **kwargs: Any) -> Any:
            raise AssertionError("no request may be made for a rejected url")

    monkeypatch.setattr(fetch.httpx, "Client", NoRequestClient)
