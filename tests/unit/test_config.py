"""Settings-load tests: documented defaults and environment overrides."""

from __future__ import annotations

import pytest

from app import config


def test_tool_limits_have_the_documented_defaults(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in (
        "MAX_TOOL_TURNS",
        "MAX_RESULT_CHARS",
        "MAX_RESULT_ITEMS",
        "MAX_ARGS_CHARS",
        "AGENT_MODELS",
    ):
        monkeypatch.delenv(name, raising=False)

    loaded = config.load_settings()

    assert loaded.max_tool_turns == 3
    assert loaded.max_result_chars == 6000
    assert loaded.max_result_items == 10
    assert loaded.max_args_chars == 500
    assert loaded.agent_models == {}


def test_tool_limits_are_loaded_from_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("MAX_TOOL_TURNS", "5")
    monkeypatch.setenv("MAX_RESULT_CHARS", "1234")
    monkeypatch.setenv("MAX_RESULT_ITEMS", "4")
    monkeypatch.setenv("MAX_ARGS_CHARS", "99")

    loaded = config.load_settings()

    assert loaded.max_tool_turns == 5
    assert loaded.max_result_chars == 1234
    assert loaded.max_result_items == 4
    assert loaded.max_args_chars == 99


def test_agent_models_parse_once_and_malformed_entries_are_skipped(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.setenv(
        "AGENT_MODELS",
        " researcher = qwen2.5:7b , writer= , broken , checker=qwen2.5:14b",
    )

    with caplog.at_level("WARNING", logger="sol_o.config"):
        loaded = config.load_settings()

    assert loaded.agent_models == {
        "researcher": "qwen2.5:7b",
        "checker": "qwen2.5:14b",
    }
    assert caplog.text.count("AGENT_MODELS") >= 2


def test_agent_models_skips_a_key_without_an_agent_prompt(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.setenv("AGENT_MODELS", "resercher=qwen2.5:7b,sun=qwen2.5:14b")

    with caplog.at_level("WARNING", logger="sol_o.config"):
        loaded = config.load_settings()

    assert loaded.agent_models == {"sun": "qwen2.5:14b"}
    assert "unknown AGENT_MODELS agent 'resercher'" in caplog.text
