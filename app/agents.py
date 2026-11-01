"""Shared agent execution: prompt loading, JSON parsing, validation, and retries.

Both the Phase 1 ``/run`` endpoint and the Phase 2 Sun orchestrator use this
module so the output contract and retry behavior exist in exactly one place.
"""

from __future__ import annotations

import json
import logging
import re
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Generic, TypeVar

from pydantic import BaseModel, ValidationError

from app.config import settings
from app.llm import LLMError, LLMTimeoutError, call_llm, model_for
from app.reliability import redact
from app.schemas import AgentOutput, FailureType, ToolRequests
from app.toolbox import MAX_TOOL_TURNS, Toolset

logger = logging.getLogger("sol_o.agents")

AGENTS_DIR = Path(__file__).resolve().parent.parent / "agents"
AGENT_NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")
FENCE_RE = re.compile(r"^\s*```(?:json)?\s*(.*?)\s*```\s*$", re.DOTALL | re.IGNORECASE)
PLANNER_AGENT = "sun"

OUTPUT_CONTRACT = """

## Runtime output contract (highest priority)

Return exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.
It must have these keys: agent, status, task, summary, findings, conflicts, gaps, clarification_needed.
- status: "complete", "partial", or "needs_clarification".
- findings: a list of objects with exactly claim, source_url, confidence.
- confidence: "high", "medium", or "low".
- conflicts and gaps: lists of strings (use [] when empty).
- clarification_needed: null unless status is "needs_clarification".
Do not add extra keys or enum values.
"""

STEP_CONTRACT = """

## Runtime output contract (highest priority)

Return exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.
Use the exact keys from your output format above. Do not add, rename, or omit keys.
- agent: your own name.
- status: "complete", "partial", or "needs_clarification".
- clarification_needed: one question when status is "needs_clarification", otherwise null.
Use only the allowed enum values.
"""

PLAN_CONTRACT = """

## Runtime output contract (highest priority)

Return exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.
Shape: {"steps": [{"agent": "<agent id>", "task": "<instruction>"}]}
- 1 to 7 steps, executed in order.
- Every agent must come from the available agents list in the user message.
- Do not add extra keys.
"""

RETRY_INSTRUCTION = """Your previous response could not be used: {error}

Return the corrected, complete JSON object only. No prose and no markdown fences."""

TOOL_BUDGET_INSTRUCTION = (
    "Tool budget is exhausted. Return your normal output object now; no more tools will run."
)

TOOL_RESULTS_HEADER = "Tool results (JSON):\n"

OutputModel = TypeVar("OutputModel", bound=BaseModel)


class AgentNotFoundError(LookupError):
    """No readable ``agents/<agent>.md`` exists for a valid agent name."""

    def __init__(self, agent: str) -> None:
        super().__init__(f"unknown agent: {agent}")
        self.agent = agent


class OutputParseError(ValueError):
    """The model's response was not the JSON object the agent contract requires."""

    def __init__(self, error_type: FailureType, message: str) -> None:
        super().__init__(message)
        self.error_type = error_type


class AgentRunError(RuntimeError):
    """An agent call failed: provider error, timeout, or invalid output after retries."""

    def __init__(
        self, error_type: FailureType, detail: str, *, attempts: int, raw: str = ""
    ) -> None:
        detail = redact(detail)
        super().__init__(detail)
        self.error_type = error_type
        self.kind = error_type.value
        self.detail = detail
        self.attempts = attempts
        self.raw = redact(raw)


@dataclass
class AgentRun(Generic[OutputModel]):
    """The validated output of one agent call plus how it was obtained."""

    output: OutputModel
    attempts: int
    duration_seconds: float


def load_system_prompt(agent: str) -> str:
    """Read agents/{agent}.md on every request, or raise."""
    if not AGENT_NAME_RE.fullmatch(agent):
        raise AgentNotFoundError(agent)
    path = AGENTS_DIR / f"{agent}.md"
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise AgentNotFoundError(agent) from None


def _decode(raw: str) -> Any:
    """Strip optional markdown fences and parse the JSON payload."""
    text = raw.strip()
    fenced = FENCE_RE.match(text)
    if fenced:
        text = fenced.group(1).strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise OutputParseError(
            FailureType.INVALID_JSON,
            f"invalid JSON: {exc.msg} (line {exc.lineno}, column {exc.colno})",
        ) from exc


def parse_output(raw: str, model: type[OutputModel]) -> OutputModel:
    """Strip optional markdown fences, parse JSON, and validate against ``model``."""
    data = _decode(raw)
    try:
        return model.model_validate(data)
    except ValidationError as exc:
        raise OutputParseError(
            FailureType.SCHEMA_MISMATCH, f"schema validation failed: {exc}"
        ) from exc


def parse_agent_output(raw: str) -> AgentOutput:
    """Validate a raw response against the Phase 1 researcher contract."""
    return parse_output(raw, AgentOutput)


def _short(text: str, limit: int = 200) -> str:
    collapsed = " ".join(text.split())
    return collapsed if len(collapsed) <= limit else collapsed[:limit] + "..."


def call_validated(
    system_prompt: str,
    user_content: str,
    model: type[OutputModel],
    contract: str,
    response_schema: dict | None = None,
    timeout_seconds: float | None = None,
    on_attempt: Callable[[int], None] | None = None,
    agent: str = "",
    toolset: Toolset | None = None,
) -> tuple[OutputModel, int]:
    """Call the LLM with optional tool turns, validate, and retry on bad output.

    A planet may answer with a ``{"tool_requests": [...]}`` envelope instead of
    its normal output. The Sun runs those tools, appends the results, and calls
    the planet again, for at most ``MAX_TOOL_TURNS`` turns per step.
    """
    tool_section = toolset.contract() if toolset is not None else ""
    messages: list[dict[str, str]] = [
        {"role": "system", "content": system_prompt + contract + tool_section},
        {"role": "user", "content": user_content},
    ]

    if response_schema is None:
        response_schema = model.model_json_schema()

    request_model = model_for(agent) if agent else None
    max_attempts = 1 + max(0, settings.llm_max_retries)
    max_calls = max_attempts + (MAX_TOOL_TURNS if toolset else 0)
    tool_turns = 0
    attempts = 0
    calls = 0

    while calls < max_calls:
        calls += 1
        if on_attempt is not None:
            on_attempt(calls)
        kwargs: dict[str, Any] = {
            "response_schema": response_schema,
            "timeout": timeout_seconds,
        }
        if request_model is not None:
            kwargs["model"] = request_model
        try:
            raw = call_llm(messages, **kwargs)
        except LLMTimeoutError as exc:
            raise AgentRunError(
                FailureType.TIMEOUT, str(exc), attempts=max(calls, exc.attempts)
            ) from exc
        except LLMError as exc:
            raise AgentRunError(
                FailureType.LLM_ERROR, str(exc), attempts=max(calls, exc.attempts)
            ) from exc

        error: OutputParseError | None = None
        try:
            data = _decode(raw)
        except OutputParseError as exc:
            error = exc
        else:
            is_envelope = (
                isinstance(data, dict) and "tool_requests" in data and "status" not in data
            )
            if not is_envelope:
                try:
                    return model.model_validate(data), calls
                except ValidationError as exc:
                    error = OutputParseError(
                        FailureType.SCHEMA_MISMATCH, f"schema validation failed: {exc}"
                    )
            elif toolset is None:
                error = OutputParseError(
                    FailureType.SCHEMA_MISMATCH,
                    "tool_requests is not available for this agent; "
                    "return the normal output object",
                )
            else:
                try:
                    requests = ToolRequests.model_validate(data)
                except ValidationError as exc:
                    error = OutputParseError(
                        FailureType.SCHEMA_MISMATCH, f"invalid tool_requests: {exc}"
                    )
                else:
                    if tool_turns >= MAX_TOOL_TURNS or calls >= max_calls:
                        error = OutputParseError(
                            FailureType.SCHEMA_MISMATCH, TOOL_BUDGET_INSTRUCTION
                        )
                        logger.warning(
                            "agent=%s requested a tool after %d tool turns",
                            agent or "?",
                            tool_turns,
                        )
                    else:
                        tool_turns += 1
                        results = [toolset.run(item) for item in requests.tool_requests]
                        messages.append({"role": "assistant", "content": raw})
                        messages.append(
                            {
                                "role": "user",
                                "content": TOOL_RESULTS_HEADER
                                + json.dumps(results, ensure_ascii=False, indent=2),
                            }
                        )
                        logger.info(
                            "agent=%s tool_turn=%d tools=%s",
                            agent or "?",
                            tool_turns,
                            ",".join(item.tool for item in requests.tool_requests),
                        )
                        continue

        if error is None:  # pragma: no cover - every branch sets an error or continues
            raise AssertionError("unreachable")
        attempts += 1
        if attempts >= max_attempts or calls >= max_calls:
            raise AgentRunError(error.error_type, str(error), attempts=calls, raw=raw)
        logger.warning(
            "invalid output attempt=%d/%d type=%s: %s",
            attempts,
            max_attempts,
            error.error_type.value,
            _short(str(error), 500),
        )
        messages.append({"role": "assistant", "content": raw})
        messages.append({"role": "user", "content": RETRY_INSTRUCTION.format(error=error)})

    # Every exit above returns or raises; this guards a future control-flow change.
    raise AgentRunError(
        FailureType.INVALID_JSON,
        f"no usable output after {calls} LLM call(s)",
        attempts=calls,
    )


def run_agent(
    agent: str,
    user_content: str,
    model: type[OutputModel],
    *,
    contract: str = STEP_CONTRACT,
    task: str = "",
    response_schema: dict | None = None,
    timeout_seconds: float | None = None,
    on_attempt: Callable[[int], None] | None = None,
    toolset: Toolset | None = None,
) -> AgentRun[OutputModel]:
    """Load ``agents/<agent>.md``, run one agent with retries, and log the attempt."""
    started = time.perf_counter()
    try:
        system_prompt = load_system_prompt(agent)
        output, attempts = call_validated(
            system_prompt,
            user_content,
            model,
            contract,
            response_schema,
            timeout_seconds,
            on_attempt=on_attempt,
            agent=agent,
            toolset=toolset,
        )
    except AgentRunError as exc:
        logger.error(
            "agent=%s task=%r attempts=%d duration=%.2fs result=%s",
            agent,
            _short(task),
            exc.attempts,
            time.perf_counter() - started,
            exc.kind,
        )
        raise

    duration = time.perf_counter() - started
    logger.info(
        "agent=%s task=%r attempts=%d duration=%.2fs result=ok",
        agent,
        _short(task),
        attempts,
        duration,
    )
    return AgentRun(output=output, attempts=attempts, duration_seconds=duration)
