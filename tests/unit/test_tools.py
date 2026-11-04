"""Tool tests: splitting, DRY_RUN, search errors, and the planet tool implementations."""

from __future__ import annotations

import socket
from pathlib import Path

import pytest

from app import logger as run_store
from app.config import settings as config_settings
from tests.unit.fakes import Page, forbid_dns, forbid_http, install_http
from tools import calc, fetch, kb, recall, telegram
from tools import search as search_tool
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


# ---------------------------------------------------------------------------
# calc
# ---------------------------------------------------------------------------


def test_calculate_handles_arithmetic_and_date_math() -> None:
    assert calc.calculate("3 * 19.99")["value"] == "59.97"
    assert calc.calculate("round(10 / 3, 2)")["value"] == "3.33"
    assert calc.calculate("days_between('2026-11-01','2026-12-01')")["value"] == "30"


@pytest.mark.parametrize(
    "expression",
    [
        "__import__('os').system('id')",
        "(1).__class__",
        "open('x')",
        "x + 2",
        "[1, 2]",
        "2 ** 99",
    ],
)
def test_calculate_rejects_anything_outside_the_allowlist(expression: str) -> None:
    with pytest.raises(calc.CalcError):
        calc.calculate(expression)


# ---------------------------------------------------------------------------
# fetch
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "url",
    [
        "file:///etc/passwd",
        "http://127.0.0.1/",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/",
        "http://user:secret@example.com/",
        "http://100.64.0.1/",
        "http://224.0.0.1/",
        "http://0.0.0.0/",
        "http://240.0.0.1/",
        "http://[::]/",
        "http://[ff02::1]/",
        "http://[2001:db8::1]/",
        "http://[2002::1]/",
        "http://[2001::1]/",
        "http://[64:ff9b::1]/",
        "http://[2606:4700::1111%25eth0]/",
        "http://[fe80::1%25eth0]/",
    ],
)
def test_fetch_url_rejects_unsafe_targets_before_any_request(
    url: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    forbid_dns(monkeypatch)
    forbid_http(monkeypatch)

    with pytest.raises(fetch.FetchError):
        fetch.fetch_url(url)


def test_fetch_url_rejects_an_unparseable_url(monkeypatch: pytest.MonkeyPatch) -> None:
    forbid_dns(monkeypatch)
    forbid_http(monkeypatch)

    with pytest.raises(fetch.FetchError):
        fetch.fetch_url("http://[::zz]/")


@pytest.mark.parametrize(
    "address",
    [
        "10.0.0.7",
        "127.0.0.1",
        "169.254.169.254",
        "100.64.0.9",
        "224.0.0.1",
        "2002::1",
        "2001::1",
        "64:ff9b::1",
        "2606:4700::1111%eth0",
        "not-an-ip",
    ],
)
def test_fetch_url_rejects_a_non_public_resolution(
    address: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        socket,
        "getaddrinfo",
        lambda *args, **kwargs: [(2, 1, 6, "", (address, 80))],
    )
    forbid_http(monkeypatch)

    with pytest.raises(fetch.FetchError, match="not publicly routable"):
        fetch.fetch_url("http://internal.example.com/")


def test_fetch_url_refuses_a_redirect_to_a_non_public_target(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        socket, "getaddrinfo", lambda *args, **kwargs: [(2, 1, 6, "", ("93.184.216.34", 80))]
    )
    requested = install_http(
        monkeypatch,
        {
            "https://example.com/start": Page(
                "https://example.com/start",
                status_code=302,
                headers={"location": "http://100.64.0.1/admin"},
            ),
        },
    )

    with pytest.raises(fetch.FetchError, match="not publicly routable"):
        fetch.fetch_url("https://example.com/start")

    assert requested == ["https://example.com/start"]


def test_fetch_url_follows_a_redirect_to_a_public_target(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        socket, "getaddrinfo", lambda *args, **kwargs: [(2, 1, 6, "", ("93.184.216.34", 80))]
    )
    requested = install_http(
        monkeypatch,
        {
            "https://example.com/start": Page(
                "https://example.com/start",
                status_code=302,
                headers={"location": "/final"},
            ),
            "https://example.com/final": Page(
                "https://example.com/final", body=b"<p>Weekend delivery.</p>"
            ),
        },
    )

    result = fetch.fetch_url("https://example.com/start")

    assert requested == ["https://example.com/start", "https://example.com/final"]
    assert result["text"] == "Weekend delivery."


def test_fetch_url_accepts_a_public_ip_literal_without_resolving(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    forbid_dns(monkeypatch)
    install_http(
        monkeypatch,
        {"http://[2606:4700::1111]/": Page("http://[2606:4700::1111]/", body=b"ok")},
    )

    result = fetch.fetch_url("http://[2606:4700::1111]/")

    assert result["status"] == 200


def test_fetch_url_returns_visible_text(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        socket, "getaddrinfo", lambda *args, **kwargs: [(2, 1, 6, "", ("93.184.216.34", 80))]
    )
    html = (
        b"<html><head><style>p{}</style></head><body><h1>Weekend</h1>"
        b"<script>bad()</script><p>We deliver on Saturdays.</p></body></html>"
    )

    install_http(
        monkeypatch,
        {"https://example.com/delivery": Page("https://example.com/delivery", body=html)},
    )

    result = fetch.fetch_url("https://example.com/delivery")

    assert result["status"] == 200
    assert "Weekend" in result["text"]
    assert "We deliver on Saturdays." in result["text"]
    assert "bad()" not in result["text"]


def test_fetch_text_cap_comes_from_settings(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(config_settings, "max_result_chars", 50)
    monkeypatch.setattr(
        socket, "getaddrinfo", lambda *args, **kwargs: [(2, 1, 6, "", ("93.184.216.34", 80))]
    )
    install_http(
        monkeypatch,
        {
            "https://example.com/long": Page(
                "https://example.com/long", body=b"word " * 100
            )
        },
    )

    result = fetch.fetch_url("https://example.com/long")

    assert len(result["text"]) == 50
    assert result["truncated"] is True


# ---------------------------------------------------------------------------
# kb
# ---------------------------------------------------------------------------


def test_kb_search_ranks_a_heading_match_first(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "kb"
    root.mkdir()
    (root / "notes.md").write_text(
        "# Shipping\n\nGeneral notes mention delivery in passing.\n\n"
        "# Delivery windows\n\nWeekend delivery starts at 14:00.\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(kb, "KB_DIR", root)
    monkeypatch.setattr(kb, "_index", None)

    results = kb.search_kb("weekend delivery")

    assert results
    assert results[0]["heading"] == "Delivery windows"
    assert results[0]["path"].endswith("kb/notes.md")


def test_kb_search_returns_empty_without_a_directory(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(kb, "KB_DIR", tmp_path / "missing")
    monkeypatch.setattr(kb, "_index", None)

    assert kb.search_kb("weekend delivery") == []


def test_kb_search_finds_the_seeded_delivery_page() -> None:
    results = kb.search_kb("weekend delivery")

    assert results
    assert results[0]["path"] == "kb/delivery.md"
    assert results[0]["heading"] == "Weekend delivery"


# ---------------------------------------------------------------------------
# recall
# ---------------------------------------------------------------------------


@pytest.fixture
def recall_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "runs.db"))
    run_store.start_run("old", "do you deliver on weekends?", source="email:dana")
    run_store.save_run(
        "old",
        task="do you deliver on weekends?",
        status="complete",
        final_draft="Yes, weekend delivery costs 4.99.",
        verdict="approved",
        outputs=[],
        duration_seconds=1.0,
    )
    run_store.start_run("other", "do you deliver on weekends?", source="email:sam")
    run_store.save_run(
        "other",
        task="do you deliver on weekends?",
        status="complete",
        final_draft="Yes, weekend delivery costs 4.99.",
        verdict="approved",
        outputs=[],
        duration_seconds=1.0,
    )
    run_store.start_run("current", "do you deliver on weekends?", source="email:dana")


def test_recall_only_returns_the_same_conversation(recall_db: None) -> None:
    results = recall.recall("weekend delivery", source="email:dana", run_id="current")

    assert [item["run_id"] for item in results] == ["old"]
    assert results[0]["final_draft"].startswith("Yes, weekend delivery")


def test_recall_is_disabled_for_an_unknown_source(recall_db: None) -> None:
    assert recall.recall("weekend delivery", source="unknown", run_id="current") == []
    assert recall.recall("weekend delivery", source="", run_id="current") == []
