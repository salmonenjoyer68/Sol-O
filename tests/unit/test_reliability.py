"""Reliability tests: redaction, retry classification, backoff, deadlines."""

from __future__ import annotations

import httpx
import openai
import pytest

from app import llm, reliability
from app.config import settings
from app.reliability import Deadline, RunTimeout, call_with_retries, redact, retry_delay
from app.schemas import FailureType


def test_redact_removes_env_secrets_and_token_shapes(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("API_KEY", "super-secret-api-key-value")
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345")
    text = (
        "key=super-secret-api-key-value "
        "bot=https://api.telegram.org/bot123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345/sendMessage "
        "openai=sk-abcdefghijklmnopqrstuvwxyz"
    )

    cleaned = redact(text)

    assert "super-secret-api-key-value" not in cleaned
    assert "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345" not in cleaned
    assert "sk-abcdefghijklmnopqrstuvwxyz" not in cleaned
    assert cleaned.count("***") >= 2


def test_backoff_delays_are_exponential() -> None:
    assert [retry_delay(number) for number in (1, 2, 3, 4)] == [1.0, 2.0, 4.0, 8.0]


def test_call_with_retries_retries_only_retryable_errors() -> None:
    calls: list[int] = []
    delays: list[float] = []

    def flaky() -> str:
        calls.append(1)
        if len(calls) < 3:
            raise ConnectionError("transient")
        return "ok"

    result = call_with_retries(
        flaky,
        max_retries=2,
        retry_if=lambda exc: isinstance(exc, ConnectionError),
        sleep=delays.append,
    )

    assert result == "ok"
    assert len(calls) == 3
    assert delays == [1.0, 2.0]

    with pytest.raises(ValueError):
        call_with_retries(
            lambda: (_ for _ in ()).throw(ValueError("fatal")),
            max_retries=5,
            retry_if=lambda exc: isinstance(exc, ConnectionError),
            sleep=delays.append,
        )


def test_deadline_expires() -> None:
    deadline = Deadline(started=0.0, seconds=0.0)
    assert deadline.expired()
    assert deadline.remaining() == 0.0
    runner = Deadline(started=reliability.time.perf_counter(), seconds=60)
    assert not runner.expired()
    assert 0 < runner.remaining() <= 60


@pytest.fixture
def llm_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("BASE_URL", "http://llm.test/v1")
    monkeypatch.setenv("API_KEY", "test-key")
    monkeypatch.setenv("MODEL", "test-model")
    monkeypatch.setattr(settings, "llm_max_retries", 2)


def _request_401() -> httpx.Request:
    return httpx.Request("POST", "http://llm.test/v1/chat/completions")


def test_llm_401_fails_immediately_without_retries(
    monkeypatch: pytest.MonkeyPatch, llm_env: None
) -> None:
    calls: list[int] = []
    delays: list[float] = []

    def fail(*args: object, **kwargs: object) -> str:
        calls.append(1)
        raise openai.AuthenticationError(
            "invalid api key", response=httpx.Response(401, request=_request_401()), body=None
        )

    monkeypatch.setattr(llm, "_request", fail)
    monkeypatch.setattr(reliability.time, "sleep", delays.append)

    with pytest.raises(llm.LLMError) as info:
        llm.call_llm([{"role": "user", "content": "hi"}])

    assert len(calls) == 1
    assert delays == []
    assert info.value.status_code == 401
    assert "401" in str(info.value)


def test_llm_server_error_retries_then_succeeds(
    monkeypatch: pytest.MonkeyPatch, llm_env: None
) -> None:
    calls: list[int] = []
    delays: list[float] = []

    def flaky(*args: object, **kwargs: object) -> str:
        calls.append(1)
        if len(calls) < 3:
            raise openai.APIStatusError(
                "server error",
                response=httpx.Response(503, request=_request_401()),
                body=None,
            )
        return '{"ok": true}'

    monkeypatch.setattr(llm, "_request", flaky)
    monkeypatch.setattr(reliability.time, "sleep", delays.append)

    assert llm.call_llm([{"role": "user", "content": "hi"}]) == '{"ok": true}'
    assert len(calls) == 3
    assert delays == [1.0, 2.0]


def test_llm_timeout_retries_with_backoff_then_fails(
    monkeypatch: pytest.MonkeyPatch, llm_env: None
) -> None:
    calls: list[int] = []
    delays: list[float] = []

    def slow(*args: object, **kwargs: object) -> str:
        calls.append(1)
        raise openai.APITimeoutError(request=_request_401())

    monkeypatch.setattr(llm, "_request", slow)
    monkeypatch.setattr(reliability.time, "sleep", delays.append)

    with pytest.raises(llm.LLMTimeoutError) as info:
        llm.call_llm([{"role": "user", "content": "hi"}])

    assert len(calls) == 3
    assert delays == [1.0, 2.0]
    assert info.value.attempts == 3


def test_failure_type_enum_values() -> None:
    assert {item.value for item in FailureType} == {
        "invalid_json",
        "schema_mismatch",
        "timeout",
        "llm_error",
        "tool_error",
    }
    with pytest.raises(RunTimeout):
        raise RunTimeout("budget")


def test_redaction_filter_covers_httpx_request_urls(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    token = "9108866247:AAFakeTokenValueForRedactionTest12345"
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", token)
    caplog.set_level("INFO", logger="httpx")

    reliability.install_redaction_filter()
    import logging

    logging.getLogger("httpx").info(
        'HTTP Request: POST https://api.telegram.org/bot%s/sendMessage "OK"', token
    )

    assert token not in caplog.text
    assert "AAFakeTokenValueForRedactionTest12345" not in caplog.text
