"""CLI conversation sources: per-invocation isolation and deliberate sharing."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from app import cli as sun_cli
from app import logger as run_store
from tools import recall


@pytest.fixture(autouse=True)
def isolated_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "runs.db"))
    monkeypatch.setenv("DRY_RUN", "true")


class StubResult:
    """The part of ``OrchestrationResult`` the CLI reads."""

    status = "complete"

    def __init__(self, run_id: str) -> None:
        self.run_id = run_id

    def model_dump(self) -> dict[str, Any]:
        return {"status": self.status, "run_id": self.run_id}


class RecordingOrchestrator:
    """Fake orchestrator that records a finished run under the source it was given."""

    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    def __call__(
        self,
        task: str,
        *,
        use_llm_planner: bool = False,
        run_id: str | None = None,
        source: str = "",
    ) -> StubResult:
        assert run_id, "the CLI must resolve the run id before orchestrating"
        self.calls.append({"task": task, "run_id": run_id, "source": source})
        run_store.start_run(run_id, task, source=source)
        run_store.save_run(
            run_id,
            task=task,
            status="complete",
            final_draft=f"draft about {task}",
            verdict="approved",
            outputs=[],
            duration_seconds=0.1,
        )
        return StubResult(run_id)


@pytest.fixture
def orchestrator(monkeypatch: pytest.MonkeyPatch) -> RecordingOrchestrator:
    fake = RecordingOrchestrator()
    monkeypatch.setattr(sun_cli, "orchestrate", fake)
    return fake


def test_cli_source_flag_is_the_run_conversation_source(
    orchestrator: RecordingOrchestrator,
) -> None:
    assert sun_cli.main(["plan the weekend launch", "--source", "team-alpha"]) == 0

    call = orchestrator.calls[0]
    assert call["source"] == "team-alpha"
    assert run_store.get_run(call["run_id"])["source"] == "team-alpha"


def test_cli_default_source_is_derived_from_its_own_run(
    orchestrator: RecordingOrchestrator,
) -> None:
    assert sun_cli.main(["plan the weekend launch"]) == 0
    assert sun_cli.main(["audit the weekend launch"]) == 0

    first, second = orchestrator.calls
    assert first["source"]
    assert first["source"] != "cli"
    assert second["source"] != "cli"
    assert first["run_id"] in first["source"]
    assert second["run_id"] in second["source"]
    assert first["source"] != second["source"]
    assert run_store.get_run(first["run_id"])["source"] == first["source"]
    assert run_store.get_run(second["run_id"])["source"] == second["source"]


def test_two_default_invocations_cannot_recall_each_other(
    orchestrator: RecordingOrchestrator,
) -> None:
    assert sun_cli.main(["weekend delivery planning"]) == 0
    assert sun_cli.main(["weekend delivery audit"]) == 0

    first, second = orchestrator.calls
    assert recall.recall(
        "weekend delivery", source=first["source"], run_id=first["run_id"]
    ) == []
    assert recall.recall(
        "weekend delivery", source=second["source"], run_id=second["run_id"]
    ) == []


def test_invocations_sharing_an_explicit_source_can_recall_each_other(
    orchestrator: RecordingOrchestrator,
) -> None:
    assert sun_cli.main(["weekend delivery planning", "--source", "team-alpha"]) == 0
    assert sun_cli.main(["weekend delivery audit", "--source", "team-alpha"]) == 0

    first, second = orchestrator.calls
    found = recall.recall(
        "weekend delivery", source=second["source"], run_id=second["run_id"]
    )
    assert [row["run_id"] for row in found] == [first["run_id"]]


def test_empty_and_unknown_sources_still_disable_recall(
    orchestrator: RecordingOrchestrator,
) -> None:
    assert sun_cli.main(["weekend delivery planning", "--source", ""]) == 0
    assert sun_cli.main(["weekend delivery audit", "--source", ""]) == 0
    assert sun_cli.main(["weekend delivery planning", "--source", "unknown"]) == 0
    assert sun_cli.main(["weekend delivery audit", "--source", "unknown"]) == 0

    for call in orchestrator.calls:
        assert recall.recall(
            "weekend delivery", source=call["source"], run_id=call["run_id"]
        ) == []
