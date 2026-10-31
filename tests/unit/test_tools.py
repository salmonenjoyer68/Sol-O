"""Tool tests: message splitting, DRY_RUN, credential handling, search errors."""

from __future__ import annotations

import pytest

from tools import search as search_tool
from tools import telegram
from tools.telegram import TelegramError, send_message, split_message


def test_split_message_never_exceeds_limit() -> None:
    text = "\n\n".join(f"paragraph {index} " + "x" * 900 for index in range(10))

    parts = split_message(text)

    assert len(parts) > 1
    assert all(0 < len(part) <= telegram.TELEGRAM_LIMIT for part in parts)


def test_split_message_hard_wraps_a_single_huge_paragraph() -> None:
    parts = split_message("y" * 9000)

    assert [len(part) for part in parts] == [telegram.TELEGRAM_LIMIT, telegram.TELEGRAM_LIMIT, 1000]


def test_send_message_dry_run_prints_instead_of_sending(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setenv("DRY_RUN", "true")
    monkeypatch.delenv("TELEGRAM_BOT_TOKEN", raising=False)
    monkeypatch.delenv("TELEGRAM_CHAT_ID", raising=False)

    result = send_message("hello operator")

    out = capsys.readouterr().out
    assert result == {"dry_run": True, "delivered": False, "parts": 1}
    assert "DRY_RUN telegram message" in out
    assert "hello operator" in out


def test_send_message_requires_credentials_when_not_dry_run(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("DRY_RUN", "false")
    monkeypatch.delenv("TELEGRAM_BOT_TOKEN", raising=False)
    monkeypatch.delenv("TELEGRAM_CHAT_ID", raising=False)

    with pytest.raises(TelegramError):
        send_message("hello operator")


def test_search_without_key_raises_search_unavailable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("TAVILY_API_KEY", raising=False)

    with pytest.raises(search_tool.SearchUnavailable):
        search_tool.search("weekend delivery")


class FakeResponse:
    def __init__(self, payload: dict[str, object]) -> None:
        self.payload = payload

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict[str, object]:
        return self.payload


def test_search_parses_tavily_results_and_sends_the_key(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("TAVILY_API_KEY", "test-key")
    captured: dict[str, object] = {}

    def fake_post(url: str, **kwargs: object) -> FakeResponse:
        captured["url"] = url
        captured["kwargs"] = kwargs
        return FakeResponse(
            {
                "results": [
                    {
                        "title": "Delivery",
                        "url": "https://example.com/delivery",
                        "content": "We deliver on Saturdays.",
                    },
                    {"title": "missing url", "content": "dropped"},
                ]
            }
        )

    monkeypatch.setattr(search_tool.httpx, "post", fake_post)

    results = search_tool.search("weekend delivery", max_results=3)

    assert results == [
        {
            "title": "Delivery",
            "url": "https://example.com/delivery",
            "content": "We deliver on Saturdays.",
        }
    ]
    kwargs = captured["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["headers"] == {"Authorization": "Bearer test-key"}
    assert kwargs["json"]["max_results"] == 3


def test_search_http_error_raises_search_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TAVILY_API_KEY", "test-key")

    def failing_post(url: str, **kwargs: object) -> FakeResponse:
        raise search_tool.httpx.ConnectError("boom")

    monkeypatch.setattr(search_tool.httpx, "post", failing_post)

    with pytest.raises(search_tool.SearchUnavailable):
        search_tool.search("weekend delivery")
