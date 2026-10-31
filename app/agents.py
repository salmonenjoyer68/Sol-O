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
from typing import Generic, TypeVar

from pydantic import BaseModel, ValidationError

from app.config import settings
from app.llm import LLMError, LLMTimeoutError, call_llm
from app.reliability import redact
from app.schemas import AgentOutput, FailureType

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


def parse_output(raw: str, model: type[OutputModel]) -> OutputModel:
    """Strip optional markdown fences, parse JSON, and validate against ``model``."""
    text = raw.strip()
    fenced = FENCE_RE.match(text)
    if fenced:
        text = fenced.group(1).strip()
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise OutputParseError(
            FailureType.INVALID_JSON,
            f"invalid JSON: {exc.msg} (line {exc.lineno}, column {exc.colno})",
        ) from exc
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
) -> tuple[OutputModel, int]:
    """Call the LLM, validate, and feed validation errors back for up to N retries."""
    messages: list[dict[str, str]] = [
        {"role": "system", "content": system_prompt + contract},
        {"role": "user", "content": user_content},
    ]

    if response_schema is None:
        response_schema = model.model_json_schema()

    max_attempts = 1 + max(0, settings.llm_max_retries)
    for attempt in range(1, max_attempts + 1):
        if on_attempt is not None:
            on_attempt(attempt)
        try:
            raw = call_llm(
                messages, response_schema=response_schema, timeout=timeout_seconds
            )
        except LLMTimeoutError as exc:
            raise AgentRunError(
                FailureType.TIMEOUT, str(exc), attempts=max(attempt, exc.attempts)
            ) from exc
        except LLMError as exc:
            raise AgentRunError(
                FailureType.LLM_ERROR, str(exc), attempts=max(attempt, exc.attempts)
            ) from exc

        try:
            return parse_output(raw, model), attempt
        except OutputParseError as exc:
            if attempt == max_attempts:
                raise AgentRunError(
                    exc.error_type, str(exc), attempts=attempt, raw=raw
                ) from None
            logger.warning(
                "invalid output attempt=%d/%d type=%s: %s",
                attempt,
                max_attempts,
                exc.error_type.value,
                _short(str(exc), 500),
            )
            messages.append({"role": "assistant", "content": raw})
            messages.append({"role": "user", "content": RETRY_INSTRUCTION.format(error=exc)})

    # The loop always returns or raises on its final iteration.
    raise AssertionError("unreachable")


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
