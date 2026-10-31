"""Phase 6 tests: event bus sequencing, persistence, replay and streaming."""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from app import approvals, events
from app import logger as store
from app.main import app


@pytest.fixture(autouse=True)
def isolated_events(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "events.db"))
    events.bus.reset()
    yield
    events.bus.reset()


def test_bus_stamps_per_run_sequence_and_persists() -> None:
    first = events.bus.publish("run-a", "run_started", {"task": "do the thing"})
    second = events.bus.publish("run-a", "plan_ready", {"agents": ["researcher"]})
    other = events.bus.publish("run-b", "run_started", {"task": "other"})

    assert [first["seq"], second["seq"], other["seq"]] == [1, 2, 1]
    assert first["id"] < second["id"] < other["id"]
    assert first["ts"]

    saved = store.get_run_events("run-a")
    assert [item["event"] for item in saved] == ["run_started", "plan_ready"]
    assert saved[0]["data"] == {"task": "do the thing"}
    assert saved[1]["seq"] == 2


def test_emit_requires_a_run_context_and_redacts_secrets(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("TAVILY_API_KEY", "tvly-secret-123456")

    assert events.emit("ignored", value=1) is None
    assert store.get_run_events("run-c") == []

    with events.run_context("run-c"):
        assert events.current_run_id() == "run-c"
        events.emit("agent_done", summary="token tvly-secret-123456")

    assert events.current_run_id() is None
    saved = store.get_run_events("run-c")
    assert saved[0]["data"] == {"summary": "token ***"}


def test_subscribers_independently_get_backlog_and_live_events() -> None:
    earlier = events.bus.publish("run-d", "run_started", {"task": "x"})
    later = events.bus.publish("run-d", "agent_called", {"step": 1, "agent": "researcher"})

    async def scenario() -> None:
        loop = asyncio.get_running_loop()
        first = events.bus.subscribe(loop, last_event_id=earlier["id"])
        second = events.bus.subscribe(loop, last_event_id=0)
        try:
            assert (await asyncio.wait_for(first.queue.get(), 1))["id"] == later["id"]
            replayed = await asyncio.wait_for(second.queue.get(), 1)
            assert replayed["id"] == earlier["id"]

            events.bus.publish("run-d", "agent_done", {"step": 1, "agent": "researcher"})
            assert (await asyncio.wait_for(first.queue.get(), 1))["event"] == "agent_done"
            assert (await asyncio.wait_for(second.queue.get(), 1))["event"] == "agent_called"
        finally:
            events.bus.unsubscribe(first.id)
            events.bus.unsubscribe(second.id)

    asyncio.run(scenario())


def test_sse_endpoint_returns_buffered_events_and_the_ui_is_served() -> None:
    store.start_run("run-e", "replay me")
    events.bus.publish("run-e", "run_started", {"task": "replay me"})

    client = TestClient(app)
    saved = client.get("/runs/run-e/events")
    assert saved.status_code == 200
    assert [item["event"] for item in saved.json()] == ["run_started"]
    assert client.get("/runs/missing/events").status_code == 404

    page = client.get("/")
    assert page.status_code == 200
    assert "<canvas id=\"sky\">" in page.text
    assert "EventSource" in page.text


def test_approve_publishes_approval_decided(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        "app.approvals.send_message",
        lambda text: {"dry_run": True, "delivered": False, "parts": 1},
    )
    store.start_run("run-f", "approve me")
    action_id = store.create_pending_action(
        "run-f", "telegram", {"message": "hello", "run_status": "complete"}
    )

    approvals.approve(action_id)

    saved = store.get_run_events("run-f")
    decided = [item for item in saved if item["event"] == "approval_decided"]
    assert len(decided) == 1
    assert decided[0]["data"] == {
        "pending_id": action_id,
        "decision": "approved",
        "run_status": "complete",
    }
