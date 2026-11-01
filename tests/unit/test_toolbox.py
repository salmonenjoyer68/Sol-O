"""Tool-loop tests: envelope handling, allowlist, turn cap, audit, model routing."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from app import agents as agents_module
from app import logger as run_store
from app import orchestrator, toolbox
from app.config import settings as config_settings
from app.reliability import Deadline, run_deadline
from app.schemas import FailureType, WriterOutput
from app.toolbox import Tool, Toolset

RUN_ID = "run-tool-test"

WRITER = {
    "agent": "writer",
    "status": "complete",
    "draft": "We deliver on weekends and it costs 4.99.",
    "facts_used": ["Weekend delivery costs 4.99."],
    "notes": "",
    "clarification_needed": None,
}

RESEARCHER = {
    "agent": "researcher",
    "status": "complete",
    "task": "weekend delivery",
    "summary": "Weekend delivery exists.",
    "findings": [
        {
            "claim": "Weekend delivery exists.",
            "source_url": "https://example.com/delivery",
            "confidence": "high",
        }
    ],
    "conflicts": [],
    "gaps": [],
    "clarification_needed": None,
}

CHECKER = {
    "agent": "checker",
    "status": "complete",
    "verdict": "approved",
    "issues": [],
    "clarification_needed": None,
}


class ScriptedLLM:
    """Returns scripted raw responses in order and records every call."""

    def __init__(self, responses: list[str]) -> None:
        self.responses = list(responses)
        self.calls: list[list[dict[str, str]]] = []

    def __call__(
        self,
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
        model: str | None = None,
    ) -> str:
        self.calls.append([dict(message) for message in messages])
        return self.responses.pop(0)

    def user_turns(self) -> list[str]:
        return [call[-1]["content"] for call in self.calls]


@pytest.fixture(autouse=True)
def isolated_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "runs.db"))
    monkeypatch.setenv("DRY_RUN", "true")
    monkeypatch.delenv("AGENT_MODELS", raising=False)
    run_store.start_run(RUN_ID, "customer asks about weekend delivery", source="email:dana")


def envelope(tool: str, **args: Any) -> str:
    return json.dumps({"tool_requests": [{"tool": tool, "args": args}]})


def make_toolset(
    monkeypatch: pytest.MonkeyPatch,
    handler: Any,
    *,
    agent: str = "writer",
    name: str = "kb_search",
    deadline: Deadline | None = None,
    events: list[tuple[str, dict[str, Any]]] | None = None,
) -> Toolset:
    monkeypatch.setitem(
        toolbox.TOOLS,
        name,
        Tool(
            name=name,
            signature='{"query": str}',
            description="test tool",
            required=(),
            handler=handler,
        ),
    )
    return Toolset(
        agent=agent,
        step=1,
        run_id=RUN_ID,
        source="email:dana",
        deadline=deadline or run_deadline(60),
        emit=lambda event, **data: events.append((event, data)) if events is not None else None,
    )


def test_envelope_runs_the_tool_then_returns_the_validated_output(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any], _ctx: Any) -> list[dict[str, str]]:
        calls.append(args)
        return [{"path": "kb/delivery.md", "text": "Weekend delivery costs 4.99."}]

    events: list[tuple[str, dict[str, Any]]] = []
    toolset = make_toolset(monkeypatch, handler, events=events)
    fake = ScriptedLLM(
        [envelope("kb_search", query="weekend"), json.dumps(WRITER)]
    )
    monkeypatch.setattr(agents_module, "call_llm", fake)

    run = agents_module.run_agent("writer", "write the reply", WriterOutput, toolset=toolset)

    assert run.output.draft == WRITER["draft"]
    assert run.attempts == 2
    assert calls == [{"query": "weekend"}]
    assert "Tool results (JSON):" in fake.user_turns()[1]
    assert "kb/delivery.md" in fake.user_turns()[1]
    assert "## Tools you may call" in fake.calls[0][0]["content"]
    assert [event for event, _ in events] == ["tool_requested", "tool_done"]

    detail = run_store.get_run_detail(RUN_ID)
    assert detail is not None
    rows = detail["tool_calls"]
    assert [(row["tool"], row["status"]) for row in rows] == [("kb_search", "ok")]
    assert "weekend" in rows[0]["args"]


def test_tool_turn_cap_stops_execution_and_demands_a_final_answer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any], _ctx: Any) -> str:
        calls.append(args)
        return "still working"

    toolset = make_toolset(monkeypatch, handler)
    fake = ScriptedLLM([envelope("kb_search", query="weekend")] * 10)
    monkeypatch.setattr(agents_module, "call_llm", fake)

    with pytest.raises(agents_module.AgentRunError) as excinfo:
        agents_module.run_agent("writer", "write the reply", WriterOutput, toolset=toolset)

    assert excinfo.value.error_type is FailureType.SCHEMA_MISMATCH
    assert len(calls) == toolbox.MAX_TOOL_TURNS
    assert excinfo.value.attempts == 6
    assert len(fake.calls) == 6
    assert any(
        "Tool budget is exhausted" in turn for turn in fake.user_turns()
    )


def test_unknown_tool_is_refused_without_running_anything(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any], _ctx: Any) -> str:
        calls.append(args)
        return "unused"

    toolset = make_toolset(monkeypatch, handler)
    fake = ScriptedLLM([envelope("delete_everything"), json.dumps(WRITER)])
    monkeypatch.setattr(agents_module, "call_llm", fake)

    run = agents_module.run_agent("writer", "write the reply", WriterOutput, toolset=toolset)

    assert run.output.draft == WRITER["draft"]
    assert calls == []
    assert '"ok": false' in fake.user_turns()[1]
    assert "tool not allowed" in fake.user_turns()[1]
    detail = run_store.get_run_detail(RUN_ID)
    assert detail is not None
    assert [row["status"] for row in detail["tool_calls"]] == ["failed"]


def test_a_failing_tool_degrades_and_is_audited(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def handler(_args: dict[str, Any], _ctx: Any) -> str:
        raise RuntimeError("Tavily is down")

    toolset = make_toolset(monkeypatch, handler)
    fake = ScriptedLLM([envelope("kb_search", query="weekend"), json.dumps(WRITER)])
    monkeypatch.setattr(agents_module, "call_llm", fake)

    run = agents_module.run_agent("writer", "write the reply", WriterOutput, toolset=toolset)

    assert run.output.draft == WRITER["draft"]
    assert "Tavily is down" in fake.user_turns()[1]
    detail = run_store.get_run_detail(RUN_ID)
    assert detail is not None
    row = detail["tool_calls"][0]
    assert row["status"] == "failed"
    assert "Tavily is down" in row["error_message"]


def test_expired_budget_refuses_to_run_a_tool(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[dict[str, Any]] = []
    toolset = make_toolset(
        monkeypatch,
        lambda args, _ctx: calls.append(args) or "ran",
        deadline=run_deadline(-1),
    )
    fake = ScriptedLLM([envelope("kb_search", query="weekend"), json.dumps(WRITER)])
    monkeypatch.setattr(agents_module, "call_llm", fake)

    agents_module.run_agent("writer", "write the reply", WriterOutput, toolset=toolset)

    assert calls == []
    assert "run_out_of_time" in fake.user_turns()[1]


def test_an_agent_without_a_toolset_cannot_request_tools(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake = ScriptedLLM([envelope("kb_search", query="weekend")] * 5)
    monkeypatch.setattr(agents_module, "call_llm", fake)

    with pytest.raises(agents_module.AgentRunError) as excinfo:
        agents_module.run_agent("writer", "write the reply", WriterOutput)

    assert excinfo.value.error_type is FailureType.SCHEMA_MISMATCH
    assert "tool_requests is not available" in str(excinfo.value)
    assert len(fake.calls) == 3


def test_agent_models_env_routes_one_agent(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("AGENT_MODELS", "writer=qwen2.5:14b")
    seen: list[dict[str, Any]] = []

    def fake(
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
        **extra: Any,
    ) -> str:
        seen.append({"agent_prompt": messages[0]["content"][:20], **extra})
        return json.dumps(WRITER)

    monkeypatch.setattr(agents_module, "call_llm", fake)
    agents_module.run_agent("writer", "write the reply", WriterOutput)

    assert seen[0].get("model") == "qwen2.5:14b"
    assert len(seen) == 1


class RoutedLLM:
    """Routes by system prompt and feeds the writer one tool envelope first."""

    def __init__(self) -> None:
        self.writer_calls = 0

    def __call__(
        self,
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
        **extra: Any,
    ) -> str:
        system = messages[0]["content"]
        if system.startswith("# Researcher Agent"):
            return json.dumps(RESEARCHER)
        if system.startswith("# Checker Agent"):
            return json.dumps(CHECKER)
        if system.startswith("# Writer Agent"):
            self.writer_calls += 1
            if self.writer_calls == 1:
                return envelope("kb_search", query="weekend delivery")
            return json.dumps(WRITER)
        raise AssertionError(f"unexpected prompt: {system[:40]}")


def test_orchestrate_records_source_and_executed_tools(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(config_settings, "auto_approve", True)
    monkeypatch.setattr(orchestrator, "search", lambda query, max_results=5: [])
    monkeypatch.setattr(agents_module, "call_llm", RoutedLLM())

    result = orchestrator.orchestrate(
        "A customer asks: do you deliver on weekends?", source="email:dana"
    )

    assert result.status in {"complete", "awaiting_approval"}
    detail = run_store.get_run_detail(result.run_id or "")
    assert detail is not None
    assert detail["run"]["source"] == "email:dana"
    tools = [row["tool"] for row in detail["tool_calls"]]
    assert "kb_search" in tools
    assert all(row["args"] for row in detail["tool_calls"] if row["tool"] == "kb_search")
