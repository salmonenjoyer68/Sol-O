"""Rendered system-prompt contract tests.

Every assertion here reads the system prompt a planet actually receives from
``run_agent`` with its per-step toolset, not the source text of ``agents/*.md``.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

import pytest

from app import agents as agents_module
from app.reliability import run_deadline
from app.schemas import (
    AgentOutput,
    CheckerOutput,
    ToolRequests,
    WriterOutput,
)
from app.toolbox import AGENT_TOOLS, TOOLS, Toolset

RUN_ID = "run-prompt-contract"
PLANETS = ("researcher", "writer", "checker")

OUTPUTS: dict[str, tuple[type, dict[str, Any]]] = {
    "researcher": (
        AgentOutput,
        {
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
        },
    ),
    "writer": (
        WriterOutput,
        {
            "agent": "writer",
            "status": "complete",
            "draft": "We deliver on weekends.",
            "facts_used": ["Weekend delivery exists."],
            "notes": "",
            "clarification_needed": None,
        },
    ),
    "checker": (
        CheckerOutput,
        {
            "agent": "checker",
            "status": "complete",
            "verdict": "approved",
            "issues": [],
            "clarification_needed": None,
        },
    ),
}

TOOL_LINE_RE = re.compile(r"^- ([a-z_]+) \{", re.MULTILINE)
ENVELOPE_SAMPLE_RE = re.compile(r'\{"tool_requests": \[\{"tool": "<name>", "args": \{\.\.\.\}\}\]\}')


class CapturingLLM:
    """Records the system prompt of every call and returns one valid output."""

    def __init__(self, reply: dict[str, Any]) -> None:
        self.reply = reply
        self.system_prompts: list[str] = []

    def __call__(
        self,
        messages: list[dict[str, str]],
        response_schema: dict | None = None,
        timeout: float | None = None,
        **extra: Any,
    ) -> str:
        self.system_prompts.append(messages[0]["content"])
        return json.dumps(self.reply)


@pytest.fixture(autouse=True)
def isolated_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SOL_O_DB", str(tmp_path / "runs.db"))


def rendered_system_prompt(monkeypatch: pytest.MonkeyPatch, agent: str) -> str:
    model, reply = OUTPUTS[agent]
    fake = CapturingLLM(reply)
    monkeypatch.setattr(agents_module, "call_llm", fake)
    toolset = Toolset(
        agent=agent,
        step=1,
        run_id=RUN_ID,
        source="email:dana",
        deadline=run_deadline(60),
        emit=lambda *_args, **_kwargs: None,
    )
    agents_module.run_agent(agent, "do the task", model, toolset=toolset)
    assert fake.system_prompts, "run_agent never called the LLM"
    return fake.system_prompts[0]


def rendered_allowlist(prompt: str) -> set[str]:
    return set(TOOL_LINE_RE.findall(prompt))


@pytest.mark.parametrize("agent", PLANETS)
def test_rendered_prompt_offers_exactly_the_planets_allowlist(
    monkeypatch: pytest.MonkeyPatch, agent: str
) -> None:
    prompt = rendered_system_prompt(monkeypatch, agent)

    assert rendered_allowlist(prompt) == set(AGENT_TOOLS[agent])


@pytest.mark.parametrize("agent", PLANETS)
def test_rendered_prompt_names_no_tool_outside_the_allowlist(
    monkeypatch: pytest.MonkeyPatch, agent: str
) -> None:
    prompt = rendered_system_prompt(monkeypatch, agent)
    disallowed = set(TOOLS) - set(AGENT_TOOLS[agent])

    for name in disallowed:
        assert name not in prompt, f"{agent} prompt promises disallowed tool {name}"


@pytest.mark.parametrize("agent", PLANETS)
def test_rendered_prompt_states_the_envelope_alongside_the_output_contract(
    monkeypatch: pytest.MonkeyPatch, agent: str
) -> None:
    prompt = rendered_system_prompt(monkeypatch, agent)
    allowed = sorted(AGENT_TOOLS[agent])

    assert "Runtime output contract" in prompt
    assert ENVELOPE_SAMPLE_RE.search(prompt), "the envelope shape is not stated"
    assert "Tool results (JSON):" in prompt
    assert "no `status` key" in prompt
    # The schema and the rendered contract describe the same envelope.
    sample = '{"tool_requests": [{"tool": "' + allowed[0] + '", "args": {}}]}'
    envelope = ToolRequests.model_validate_json(sample)
    assert envelope.tool_requests[0].tool == allowed[0]


def test_researcher_prompt_explains_injected_web_results_and_its_tools(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    prompt = rendered_system_prompt(monkeypatch, "researcher").lower()

    assert "no search tool of your own" not in prompt
    assert "do not ask for more searches" not in prompt
    assert "web search results (json)" in prompt
    assert "inject" in prompt
    for name in AGENT_TOOLS["researcher"]:
        assert name in prompt


def test_writer_prompt_allows_lookups_but_forbids_new_facts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    prompt = rendered_system_prompt(monkeypatch, "writer")
    lowered = prompt.lower()

    assert "do not research" not in lowered
    assert "never add new factual claims" in lowered
    assert "numbers" in lowered and "quotes" in lowered and "sources" in lowered
    for name in AGENT_TOOLS["writer"]:
        assert name in prompt


def test_checker_prompt_documents_its_allowed_tools(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    prompt = rendered_system_prompt(monkeypatch, "checker")

    assert "kb_search" in prompt
    assert "calc" in prompt
    assert "fetch_url" not in prompt
    assert "recall" not in prompt
