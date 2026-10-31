"""Orchestrator tests with a fake LLM: plans, revision loop, logger, notifier, API."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from app import approvals, orchestrator
from app import cli as sun_cli
from app import logger as run_store
from app.config import settings as config_settings
from app.main import app
from app.schemas import FailureType
from tools.search import SearchUnavailable

RESEARCHER = {
    "agent": "researcher",
    "status": "complete",
    "task": "Explain why small cafes should use online ordering.",
    "summary": "Online ordering reduces phone errors and captures demand outside opening hours.",
    "findings": [
        {
            "claim": "Online ordering reduces phone-order mistakes.",
            "source_url": "https://example.com/online-ordering",
            "confidence": "medium",
        }
    ],
    "conflicts": [],
    "gaps": ["No source verifies the demand claim."],
    "clarification_needed": None,
}

WRITER = {
    "agent": "writer",
    "status": "complete",
    "draft": "Subject: Start taking orders online. It reduces phone-order mistakes.",
    "facts_used": ["Online ordering reduces phone-order mistakes."],
    "notes": "",
    "clarification_needed": None,
}

CHECKER = {
    "agent": "checker",
    "status": "complete",
    "verdict": "approved",
    "issues": [],
    "clarification_needed": None,
}

CHECKER_NEEDS_CHANGES = {
    "agent": "checker",
    "status": "complete",
    "verdict": "needs_changes",
    "issues": [
        {
            "problem": "The draft claims reduced mistakes without hedging.",
            "location": "It reduces phone-order mistakes.",
            "suggestion": "Say it can reduce phone-order mistakes.",
        }
    ],
    "clarification_needed": None,
}

SEARCH_RESULTS = [
    {
        "title": "Online ordering guide",
        "url": "https://example.com/online-ordering",
        "content": "Online ordering reduces phone-order mistakes.",
    }
]


def _agent_for(system: str) -> str:
    if system.startswith("# Researcher Agent"):
        return "researcher"
    if system.startswith("# Writer Agent"):
        return "writer"
    if system.startswith("# Checker Agent"):
        return "checker"
    if system.startswith("# Sun Planner"):
        return "sun"
    raise AssertionError(f"unexpected system prompt: {system[:40]}")


class FakeLLM:
    """Routes by agent prompt, records user messages, supports per-agent queues."""

    def __init__(self, planner_steps: list[dict[str, Any]] | None = None) -> None:
        self.planner_steps = planner_steps
        self.calls: list[dict[str, str]] = []
        self.outputs = {
            "researcher": RESEARCHER,
            "writer": WRITER,
            "checker": CHECKER,
        }
        self.queue: dict[str, list[dict[str, Any]]] = {}

    def __call__(
        self,
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
    ) -> str:
        name = _agent_for(messages[0]["content"])
        self.calls.append({"agent": name, "user": messages[1]["content"]})
        if name == "sun":
            return json.dumps({"steps": self.planner_steps or []})
        queued = self.queue.get(name)
        return json.dumps(queued.pop(0) if queued else self.outputs[name])

    def count(self, agent: str) -> int:
        return sum(1 for call in self.calls if call["agent"] == agent)


class SearchStub:
    def __init__(self, results: list[dict[str, str]]) -> None:
        self.results = results
        self.queries: list[str] = []

    def __call__(self, query: str, max_results: int = 5) -> list[dict[str, str]]:
        self.queries.append(query)
        return self.results


@pytest.fixture(autouse=True)
def isolated_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> SearchStub:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "runs.db"))
    monkeypatch.setenv("DRY_RUN", "true")
    monkeypatch.delenv("TAVILY_API_KEY", raising=False)
    monkeypatch.setattr(config_settings, "auto_approve", True)
    stub = SearchStub(SEARCH_RESULTS)
    monkeypatch.setattr(orchestrator, "search", stub)
    return stub


@pytest.fixture
def events(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, dict[str, Any]]]:
    collected: list[tuple[str, dict[str, Any]]] = []
    monkeypatch.setattr(orchestrator, "emit", lambda event, **data: collected.append((event, data)))
    return collected


@pytest.fixture
def notified(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    messages: list[str] = []

    def fake_send(text: str) -> dict[str, object]:
        messages.append(text)
        return {"dry_run": True, "delivered": False, "parts": 1}

    monkeypatch.setattr(orchestrator, "send_message", fake_send)
    return messages


def test_hardcoded_plan_runs_agents_then_logger_and_notifier(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
    notified: list[str],
    isolated_run: SearchStub,
) -> None:
    fake = FakeLLM()
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert result.planner == "hardcoded"
    assert [item.agent for item in result.outputs] == [
        "researcher",
        "writer",
        "checker",
        "logger",
        "notifier",
    ]
    assert [item.step for item in result.outputs] == [1, 2, 3, 4, 5]
    assert result.stopped_at is None and result.reason is None
    assert result.run_id is not None

    assert [call["agent"] for call in fake.calls] == ["researcher", "writer", "checker"]
    assert "Online ordering reduces phone-order mistakes." in fake.calls[1]["user"]
    assert "Start taking orders online" in fake.calls[2]["user"]
    assert "https://example.com/online-ordering" in fake.calls[0]["user"]
    assert isolated_run.queries == ["Explain why small cafes should use online ordering."]

    names = [name for name, _ in events if name != "agent_done"]
    assert names == [
        "run_started",
        "plan_ready",
        "agent_called",
        "agent_working",
        "search_done",
        "agent_working",
        "agent_called",
        "agent_working",
        "agent_called",
        "agent_working",
        "agent_called",
        "logger_saved",
        "agent_called",
        "agent_working",
        "notifier_sent",
        "all_done",
    ]
    assert events[0][1] == {"task": "Explain why small cafes should use online ordering."}
    plan_ready = next(data for name, data in events if name == "plan_ready")
    assert plan_ready["planner"] == "hardcoded"
    assert plan_ready["agents"] == ["researcher", "writer", "checker"]
    assert [step["agent"] for step in plan_ready["steps"]] == [
        "researcher",
        "writer",
        "checker",
    ]
    done = [data for name, data in events if name == "agent_done"]
    researcher_done = next(item for item in done if item["agent"] == "researcher")
    assert researcher_done["summary"] == RESEARCHER["summary"][:140]
    assert all(len(item["summary"]) <= 140 for item in done)

    assert len(notified) == 1
    assert "complete" in notified[0]
    assert WRITER["draft"] in notified[0]
    assert "approved" in notified[0]

    row = run_store.recent_runs()[0]
    assert row["id"] == result.run_id
    assert row["status"] == "complete"
    assert row["final_draft"] == WRITER["draft"]
    assert row["verdict"] == "approved"
    assert [item["agent"] for item in row["outputs"]] == ["researcher", "writer", "checker"]
    assert row["duration_seconds"] >= 0


def test_needs_clarification_stops_early_but_is_still_logged(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
    notified: list[str],
) -> None:
    fake = FakeLLM()
    fake.outputs["researcher"] = {
        **RESEARCHER,
        "status": "needs_clarification",
        "summary": "",
        "findings": [],
        "gaps": [],
        "clarification_needed": "What deliverable do you need?",
    }
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("do the thing")

    assert result.status == "needs_clarification"
    assert result.stopped_at == "researcher"
    assert result.reason == "What deliverable do you need?"
    assert [item.agent for item in result.outputs] == ["researcher", "logger", "notifier"]
    assert fake.count("writer") == 0
    assert (
        "stopped",
        {
            "step": 1,
            "agent": "researcher",
            "status": "needs_clarification",
            "reason": "What deliverable do you need?",
        },
    ) in events
    assert run_store.recent_runs()[0]["status"] == "needs_clarification"
    assert "needs_clarification" in notified[0]


def test_invalid_output_retries_then_stops_with_failure_and_logs(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
) -> None:
    calls: list[str] = []

    def always_invalid(
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
    ) -> str:
        calls.append(_agent_for(messages[0]["content"]))
        return "not json"

    monkeypatch.setattr("app.agents.call_llm", always_invalid)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "failed"
    assert result.stopped_at == "researcher"
    assert result.reason is not None and result.reason.startswith("invalid_json:")
    assert [item.agent for item in result.outputs] == ["logger", "notifier"]
    assert calls == ["researcher"] * 3
    names = [name for name, _ in events]
    assert "agent_failed" in names and "stopped" in names
    failed = dict(events)["agent_failed"]
    assert failed["attempts"] == 3
    assert run_store.recent_runs()[0]["status"] == "failed"


def test_retry_recovers_after_one_invalid_output(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    fake = FakeLLM()
    seen = {"researcher": 0}

    def flaky(
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
    ) -> str:
        name = _agent_for(messages[0]["content"])
        if name == "researcher":
            seen["researcher"] += 1
            if seen["researcher"] == 1:
                return "not json"
        return fake(messages, response_schema, timeout)

    monkeypatch.setattr("app.agents.call_llm", flaky)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert result.outputs[0].attempts == 2
    assert result.outputs[0].agent == "researcher"


def test_checker_needs_changes_triggers_one_revision_and_recheck(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
) -> None:
    fake = FakeLLM()
    fake.queue["checker"] = [CHECKER_NEEDS_CHANGES, CHECKER]
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert [item.agent for item in result.outputs] == [
        "researcher",
        "writer",
        "checker",
        "writer",
        "checker",
        "logger",
        "notifier",
    ]
    assert fake.count("writer") == 2
    assert fake.count("checker") == 2
    assert "revision_requested" in [name for name, _ in events]
    revision_call = fake.calls[3]
    assert "The draft claims reduced mistakes without hedging." in revision_call["user"]
    row = run_store.recent_runs()[0]
    assert row["status"] == "complete"
    assert row["verdict"] == "approved"


def test_second_checker_rejection_finishes_partial_after_one_revision(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
    notified: list[str],
) -> None:
    fake = FakeLLM()
    fake.queue["checker"] = [CHECKER_NEEDS_CHANGES, CHECKER_NEEDS_CHANGES]
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "partial"
    assert result.stopped_at == "checker"
    assert result.reason is not None and "still rejected" in result.reason
    assert fake.count("writer") == 2
    assert fake.count("checker") == 2
    assert result.outputs[-2].agent == "logger"
    assert result.outputs[-1].agent == "notifier"
    assert "needs_changes" in notified[0]
    assert "reduced mistakes without hedging" in notified[0]
    row = run_store.recent_runs()[0]
    assert row["status"] == "partial"
    assert row["verdict"] == "needs_changes"


def test_missing_search_key_continues_with_unverified_note(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
) -> None:
    def unavailable(query: str, max_results: int = 5) -> list[dict[str, str]]:
        raise SearchUnavailable("TAVILY_API_KEY is not set")

    monkeypatch.setattr(orchestrator, "search", unavailable)
    fake = FakeLLM()
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert "search_failed" in [name for name, _ in events]
    assert "unverified" in fake.calls[0]["user"]


def test_llm_planner_drops_unknown_agents_and_caps_seven_llm_steps(
    monkeypatch: pytest.MonkeyPatch,
    events: list[tuple[str, dict[str, Any]]],
    isolated_run: SearchStub,
) -> None:
    steps = [{"agent": "ghost", "task": "impossible"}] + [
        {"agent": "researcher", "task": f"research angle {index}"} for index in range(9)
    ]
    fake = FakeLLM(planner_steps=steps)
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.", True)

    assert result.planner == "llm"
    assert [step.agent for step in result.plan] == ["researcher"] * 7 + ["logger", "notifier"]
    assert result.plan[0].task == "research angle 0"
    assert [item.agent for item in result.outputs] == ["researcher"] * 7 + ["logger", "notifier"]
    assert result.status == "complete"
    assert len(isolated_run.queries) == 1
    plan_ready = next(data for name, data in events if name == "plan_ready")
    assert plan_ready == {
        "planner": "llm",
        "agents": ["researcher"] * 7,
        "steps": [step.model_dump() for step in result.plan[:7]],
    }


def test_llm_planner_drops_checker_without_an_earlier_writer(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    steps = [
        {"agent": "checker", "task": "review a draft that does not exist"},
        {"agent": "researcher", "task": "research the topic"},
        {"agent": "writer", "task": "write the explanation"},
    ]
    fake = FakeLLM(planner_steps=steps)
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.", True)

    assert [step.agent for step in result.plan] == [
        "researcher",
        "writer",
        "logger",
        "notifier",
    ]
    assert result.status == "complete"


def test_runs_endpoint_returns_recent_rows(
    monkeypatch: pytest.MonkeyPatch, notified: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    client = TestClient(app)
    orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    response = client.get("/runs")

    assert response.status_code == 200
    rows = response.json()
    assert len(rows) == 1
    assert rows[0]["status"] == "complete"
    assert rows[0]["final_draft"] == WRITER["draft"]
    assert isinstance(rows[0]["outputs"], list)
    assert "outputs_json" not in rows[0]


def test_inbound_returns_run_id_and_records_the_run(
    monkeypatch: pytest.MonkeyPatch, notified: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    client = TestClient(app)

    response = client.post(
        "/inbound", json={"message": "Do you deliver on weekends?", "source": "telegram"}
    )

    assert response.status_code == 202
    body = response.json()
    assert body["status"] == "accepted"
    run_id = body["run_id"]
    assert run_id

    rows = client.get("/runs").json()
    assert [row["id"] for row in rows] == [run_id]
    assert rows[0]["task"] == "Do you deliver on weekends?"
    assert rows[0]["status"] == "complete"


def test_orchestrate_endpoint_queues_a_background_run(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]], notified: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    response = TestClient(app).post(
        "/orchestrate",
        json={
            "task": "Explain why small cafes should use online ordering.",
            "use_llm_planner": False,
        },
    )

    assert response.status_code == 200
    body = response.json()
    assert body["status"] == "accepted"
    run_id = body["run_id"]

    # TestClient drains background tasks before returning, so the run is finished.
    detail = TestClient(app).get(f"/runs/{run_id}").json()
    assert detail["run"]["status"] == "complete"
    assert [item["agent"] for item in detail["run"]["outputs"]] == [
        "researcher",
        "writer",
        "checker",
    ]


def test_run_endpoint_still_returns_a_single_agent_output(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    response = TestClient(app).post(
        "/run", json={"agent": "researcher", "task": "Explain online ordering for cafes."}
    )

    assert response.status_code == 200
    assert response.json()["agent"] == "researcher"
    assert response.json()["status"] == "complete"


def test_planner_failure_is_logged_and_notified(
    monkeypatch: pytest.MonkeyPatch, notified: list[str]
) -> None:
    def always_invalid(
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
    ) -> str:
        return "not json"

    monkeypatch.setattr("app.agents.call_llm", always_invalid)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.", True)

    assert result.status == "failed"
    assert result.stopped_at == "sun"
    assert result.reason is not None and result.reason.startswith("invalid_json:")
    assert [item.agent for item in result.outputs] == ["logger", "notifier"]
    assert run_store.recent_runs()[0]["status"] == "failed"
    assert notified == []  # failed runs never auto-send
    assert result.outputs[-1].output["status"] == "skipped"


# --------------------------------------------------------------------------
# Phase 5: structured failures, audit rows, approval gate, CLI
# --------------------------------------------------------------------------


def _always_invalid(
    messages: list[dict[str, str]],
    response_schema: dict | None = None,
    timeout: float | None = None,
) -> str:
    return "not json"


@pytest.fixture
def pending_sends(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    """Turn approval mode on and capture what approval would send."""
    monkeypatch.setattr(config_settings, "auto_approve", False)
    monkeypatch.setattr(config_settings, "require_approval", True)
    sent: list[str] = []

    def fake_send(text: str) -> dict[str, object]:
        sent.append(text)
        return {"dry_run": True, "delivered": False, "parts": 1}

    monkeypatch.setattr("app.approvals.send_message", fake_send)
    return sent


def test_invalid_json_is_a_structured_failure_and_written_to_agent_calls(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("app.agents.call_llm", _always_invalid)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "failed"
    assert result.failed_at == "researcher"
    assert result.stopped_at == "researcher"
    assert result.failure is not None
    assert result.failure.error_type == FailureType.INVALID_JSON
    assert result.failure.attempts == 3
    assert result.failure.raw_output == "not json"

    detail = run_store.get_run_detail(result.run_id)
    assert detail is not None
    call = next(item for item in detail["agent_calls"] if item["agent"] == "researcher")
    assert call["status"] == "failed"
    assert call["error_type"] == "invalid_json"
    assert call["error_message"]
    assert call["raw_output"] == "not json"
    assert call["attempts"] == 3
    assert call["input_chars"] > 0
    assert detail["run"]["status"] == "failed"


def test_schema_mismatch_is_reported_as_its_own_error_type(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def wrong_shape(
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
    ) -> str:
        return '{"agent": "researcher"}'

    monkeypatch.setattr("app.agents.call_llm", wrong_shape)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "failed"
    assert result.failure is not None
    assert result.failure.error_type == FailureType.SCHEMA_MISMATCH
    assert "schema validation failed" in result.failure.message


def test_run_budget_exhaustion_returns_run_timeout(
    monkeypatch: pytest.MonkeyPatch, notified: list[str]
) -> None:
    monkeypatch.setattr(config_settings, "run_timeout_seconds", 0.00001)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "failed"
    assert result.reason == "run_timeout"
    assert result.failed_at == "researcher"
    assert result.failure is not None
    assert result.failure.error_type == FailureType.TIMEOUT
    assert notified == []
    assert run_store.recent_runs()[0]["status"] == "failed"


def test_long_task_is_truncated_and_warning_is_recorded(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(config_settings, "max_input_chars", 10)
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    result = orchestrator.orchestrate("x" * 40)

    assert result.warning is not None and "truncated from 40 to 10" in result.warning
    row = run_store.get_run(result.run_id)
    assert row is not None
    assert row["warning"] == result.warning
    assert row["task"] == "x" * 10


def test_search_tool_failure_is_written_to_tool_calls(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def unavailable(query: str, max_results: int = 5) -> list[dict[str, str]]:
        raise SearchUnavailable("TAVILY_API_KEY is not set")

    monkeypatch.setattr(orchestrator, "search", unavailable)
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    detail = run_store.get_run_detail(result.run_id)
    assert detail is not None
    search_calls = [item for item in detail["tool_calls"] if item["tool"] == "search"]
    assert len(search_calls) == 1
    assert search_calls[0]["status"] == "failed"
    assert "TAVILY_API_KEY" in search_calls[0]["error_message"]


def test_approval_gate_creates_pending_and_never_sends(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "awaiting_approval"
    assert result.stopped_at == "notifier"
    assert result.failure is None
    assert pending_sends == []
    assert run_store.get_run(result.run_id)["status"] == "awaiting_approval"

    pending = run_store.list_pending(24)
    assert len(pending) == 1
    payload = pending[0]["payload"]
    assert payload["run_status"] == "complete"
    assert payload["draft"] == WRITER["draft"]
    assert payload["verdict"] == "approved"
    assert WRITER["draft"] in payload["message"]

    detail = run_store.get_run_detail(result.run_id)
    assert detail is not None
    assert [item for item in detail["tool_calls"] if item["tool"] == "telegram"] == []


def test_approve_executes_exactly_once_and_marks_the_run_complete(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    action_id = run_store.list_pending(24)[0]["id"]

    outcome = approvals.approve(action_id)

    assert outcome["status"] == "approved"
    assert outcome["run_status"] == "complete"
    assert len(pending_sends) == 1
    assert WRITER["draft"] in pending_sends[0]
    assert run_store.get_run(result.run_id)["status"] == "complete"

    detail = run_store.get_run_detail(result.run_id)
    assert detail is not None
    telegram = [item for item in detail["tool_calls"] if item["tool"] == "telegram"]
    assert telegram[-1]["status"] == "ok"

    with pytest.raises(approvals.ApprovalConflict):
        approvals.approve(action_id)
    assert len(pending_sends) == 1


def test_reject_executes_nothing_and_marks_the_run_rejected(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    action_id = run_store.list_pending(24)[0]["id"]

    outcome = approvals.reject(action_id)

    assert outcome["status"] == "rejected"
    assert pending_sends == []
    assert run_store.get_run(result.run_id)["status"] == "rejected"
    with pytest.raises(approvals.ApprovalConflict):
        approvals.reject(action_id)


def test_expired_action_cannot_be_approved(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    action_id = run_store.list_pending(24)[0]["id"]
    with sqlite3.connect(run_store.db_path()) as connection:
        connection.execute(
            "UPDATE pending_actions SET created_at = ? WHERE id = ?",
            ("2000-01-01T00:00:00+00:00", action_id),
        )

    with pytest.raises(approvals.ApprovalConflict) as info:
        approvals.approve(action_id)

    assert "expired" in str(info.value)
    assert pending_sends == []
    assert run_store.get_pending(action_id)["status"] == "expired"


def test_tokens_are_redacted_from_stored_payloads(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    token = "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", token)
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())

    result = orchestrator.orchestrate(f"A customer asks about {token} and prices.")

    pending = run_store.list_pending(24)
    assert pending
    stored = json.dumps(pending[0]["payload"])
    assert token not in stored
    assert "ABCDEFGHIJKLMNOPQRSTUVWXYZ" not in stored
    run = run_store.get_run(result.run_id)
    assert run is not None
    assert token not in run["task"]


def test_runs_filter_detail_and_pending_api_endpoints(
    monkeypatch: pytest.MonkeyPatch, pending_sends: list[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    client = TestClient(app)

    awaiting = client.get("/runs", params={"status": "awaiting_approval"}).json()
    assert [row["id"] for row in awaiting] == [result.run_id]
    assert client.get("/runs", params={"status": "failed"}).json() == []

    detail = client.get(f"/runs/{result.run_id}")
    assert detail.status_code == 200
    body = detail.json()
    assert body["run"]["id"] == result.run_id
    assert [call["agent"] for call in body["agent_calls"]] == [
        "researcher",
        "writer",
        "checker",
    ]
    assert body["tool_calls"][0]["tool"] == "search"

    pending = client.get("/pending").json()
    assert len(pending) == 1
    action_id = pending[0]["id"]

    approved = client.post(f"/pending/{action_id}/approve")
    assert approved.status_code == 200
    assert approved.json()["run_status"] == "complete"
    assert client.post(f"/pending/{action_id}/approve").status_code == 409
    assert client.get("/pending").json() == []
    assert client.get("/runs/missing").status_code == 404


def test_cli_pending_show_and_approve(
    monkeypatch: pytest.MonkeyPatch,
    pending_sends: list[str],
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setattr("app.agents.call_llm", FakeLLM())
    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    action_id = run_store.list_pending(24)[0]["id"]

    assert sun_cli.main(["--pending"]) == 0
    assert f"#{action_id}" in capsys.readouterr().out

    assert sun_cli.main(["--show", result.run_id]) == 0
    timeline = capsys.readouterr().out
    assert "timeline:" in timeline
    assert "researcher" in timeline
    assert "checker" in timeline

    assert sun_cli.main(["--approve", str(action_id)]) == 0
    assert '"status": "approved"' in capsys.readouterr().out
    assert len(pending_sends) == 1
    assert sun_cli.main(["--approve", str(action_id)]) == 1


def test_cli_show_reports_failed_agent_calls(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr("app.agents.call_llm", _always_invalid)
    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")
    capsys.readouterr()

    assert sun_cli.main(["--show", result.run_id]) == 0
    output = capsys.readouterr().out
    assert "status=failed" in output
    assert "invalid_json" in output
    assert "researcher" in output
