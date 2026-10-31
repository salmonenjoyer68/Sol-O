"""Orchestrator tests with a fake LLM: plan, forwarding, retries, stop conditions."""

from __future__ import annotations

import json
from typing import Any

import pytest
from fastapi.testclient import TestClient

from app import orchestrator
from app.main import app

RESEARCHER = {
    "agent": "researcher",
    "status": "complete",
    "task": "Explain why small cafes should use online ordering.",
    "summary": "Online ordering reduces phone errors and captures demand outside opening hours.",
    "findings": [
        {
            "claim": "Online ordering reduces phone-order mistakes.",
            "source_url": "unverified",
            "confidence": "low",
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
    """Routes by agent prompt and records the first user message of every call."""

    def __init__(self, planner_steps: list[dict[str, Any]] | None = None) -> None:
        self.planner_steps = planner_steps
        self.calls: list[dict[str, str]] = []
        self.outputs = {
            "researcher": RESEARCHER,
            "writer": WRITER,
            "checker": CHECKER,
        }

    def __call__(self, messages: list[dict[str, str]], response_schema: dict | None = None) -> str:
        name = _agent_for(messages[0]["content"])
        self.calls.append({"agent": name, "user": messages[1]["content"]})
        if name == "sun":
            return json.dumps({"steps": self.planner_steps or []})
        return json.dumps(self.outputs[name])

    def count(self, agent: str) -> int:
        return sum(1 for call in self.calls if call["agent"] == agent)


@pytest.fixture
def events(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, dict[str, Any]]]:
    collected: list[tuple[str, dict[str, Any]]] = []
    monkeypatch.setattr(orchestrator, "emit", lambda event, **data: collected.append((event, data)))
    return collected


def test_hardcoded_plan_runs_agents_in_order_and_forwards_outputs(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    fake = FakeLLM()
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert result.planner == "hardcoded"
    assert [item.agent for item in result.outputs] == ["researcher", "writer", "checker"]
    assert [item.step for item in result.outputs] == [1, 2, 3]
    assert result.stopped_at is None and result.reason is None

    assert [call["agent"] for call in fake.calls] == ["researcher", "writer", "checker"]
    assert "Online ordering reduces phone-order mistakes." in fake.calls[1]["user"]
    assert "Start taking orders online" in fake.calls[2]["user"]

    names = [name for name, _ in events]
    assert names == [
        "plan_ready",
        "agent_called",
        "agent_done",
        "agent_called",
        "agent_done",
        "agent_called",
        "agent_done",
        "all_done",
    ]
    assert events[0][1]["planner"] == "hardcoded"
    assert events[-1][1]["status"] == "complete"


def test_needs_clarification_stops_before_next_agent(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
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
    assert [item.agent for item in result.outputs] == ["researcher"]
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
    assert "all_done" not in [name for name, _ in events]


def test_invalid_output_retries_then_stops_with_failure(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    calls: list[str] = []

    def always_invalid(messages: list[dict[str, str]], response_schema: dict | None = None) -> str:
        calls.append(_agent_for(messages[0]["content"]))
        return "not json"

    monkeypatch.setattr("app.agents.call_llm", always_invalid)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "failed"
    assert result.stopped_at == "researcher"
    assert result.reason is not None and result.reason.startswith("invalid_agent_output:")
    assert result.outputs == []
    assert calls == ["researcher"] * 3
    names = [name for name, _ in events]
    assert "agent_failed" in names and "stopped" in names
    failed = dict(events)["agent_failed"]
    assert failed["attempts"] == 3


def test_retry_recovers_after_one_invalid_output(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    fake = FakeLLM()
    seen = {"researcher": 0}

    def flaky(messages: list[dict[str, str]], response_schema: dict | None = None) -> str:
        name = _agent_for(messages[0]["content"])
        if name == "researcher":
            seen["researcher"] += 1
            if seen["researcher"] == 1:
                return "not json"
        return fake(messages, response_schema)

    monkeypatch.setattr("app.agents.call_llm", flaky)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.")

    assert result.status == "complete"
    assert result.outputs[0].attempts == 2
    assert result.outputs[0].agent == "researcher"


def test_llm_planner_drops_unknown_agents_and_caps_at_seven(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
) -> None:
    steps = [{"agent": "ghost", "task": "impossible"}] + [
        {"agent": "researcher", "task": f"research angle {index}"} for index in range(9)
    ]
    fake = FakeLLM(planner_steps=steps)
    monkeypatch.setattr("app.agents.call_llm", fake)

    result = orchestrator.orchestrate("Explain why small cafes should use online ordering.", True)

    assert result.planner == "llm"
    assert len(result.plan) == 7
    assert all(step.agent == "researcher" for step in result.plan)
    assert result.plan[0].task == "research angle 0"
    assert all(item.agent == "researcher" for item in result.outputs)
    assert result.status == "complete"
    assert events[0] == (
        "plan_ready",
        {"planner": "llm", "steps": [step.model_dump() for step in result.plan]},
    )


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

    assert [step.agent for step in result.plan] == ["researcher", "writer"]
    assert [item.agent for item in result.outputs] == ["researcher", "writer"]
    assert result.status == "complete"


def test_orchestrate_endpoint_returns_the_same_result_as_the_orchestrator(
    monkeypatch: pytest.MonkeyPatch, events: list[tuple[str, dict[str, Any]]]
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
    assert body["status"] == "complete"
    assert body["planner"] == "hardcoded"
    assert [item["agent"] for item in body["outputs"]] == ["researcher", "writer", "checker"]


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
