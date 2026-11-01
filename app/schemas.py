"""Pydantic contracts for the agent API and orchestration (Phases 1-5)."""

from __future__ import annotations

from enum import Enum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field


class RunRequest(BaseModel):
    """Request body for POST /run."""

    model_config = ConfigDict(extra="forbid")

    agent: str = Field(min_length=1)
    task: str = Field(min_length=1)


Status = Literal["complete", "partial", "needs_clarification"]
Confidence = Literal["high", "medium", "low"]


class Finding(BaseModel):
    """One atomic, sourced claim."""

    model_config = ConfigDict(extra="forbid")

    claim: str
    source_url: str
    confidence: Confidence


class AgentOutput(BaseModel):
    """Strict JSON contract every agent must return."""

    model_config = ConfigDict(extra="forbid")

    agent: str
    status: Status
    task: str
    summary: str
    findings: list[Finding]
    conflicts: list[str]
    gaps: list[str]
    clarification_needed: str | None


class WriterOutput(BaseModel):
    """Strict JSON contract for the writer agent."""

    model_config = ConfigDict(extra="forbid")

    agent: str
    status: Status
    draft: str
    facts_used: list[str]
    notes: str
    clarification_needed: str | None


class CheckerIssue(BaseModel):
    """One problem found by the checker."""

    model_config = ConfigDict(extra="forbid")

    problem: str
    location: str
    suggestion: str


class CheckerOutput(BaseModel):
    """Strict JSON contract for the checker agent."""

    model_config = ConfigDict(extra="forbid")

    agent: str
    status: Status
    verdict: Literal["approved", "needs_changes"]
    issues: list[CheckerIssue]
    clarification_needed: str | None


class ToolRequest(BaseModel):
    """One tool a planet asks the Sun to run."""

    model_config = ConfigDict(extra="forbid")

    tool: str = Field(min_length=1)
    args: dict[str, Any] = Field(default_factory=dict)


class ToolRequests(BaseModel):
    """The envelope a planet returns to ask the Sun to run tools."""

    model_config = ConfigDict(extra="forbid")

    tool_requests: list[ToolRequest] = Field(min_length=1)


class FailureType(str, Enum):
    """Structured failure categories recorded for every hard agent/tool failure."""

    INVALID_JSON = "invalid_json"
    SCHEMA_MISMATCH = "schema_mismatch"
    TIMEOUT = "timeout"
    LLM_ERROR = "llm_error"
    TOOL_ERROR = "tool_error"


class Failure(BaseModel):
    """One structured failure, safe to log and return to callers."""

    model_config = ConfigDict(extra="forbid")

    agent: str
    error_type: FailureType
    message: str
    raw_output: str = ""
    attempts: int = 0


class OrchestrationRequest(BaseModel):
    """Request body for POST /orchestrate."""

    model_config = ConfigDict(extra="forbid")

    task: str = Field(min_length=1)
    use_llm_planner: bool = False
    # Conversation key used to scope memory recall; empty means no conversation.
    source: str = ""


class PlanStep(BaseModel):
    """One planned agent call."""

    model_config = ConfigDict(extra="forbid")

    agent: str = Field(min_length=1)
    task: str = Field(min_length=1)


class SunPlan(BaseModel):
    """Planner output: the ordered steps for one run."""

    model_config = ConfigDict(extra="forbid")

    steps: list[PlanStep]


class StepResult(BaseModel):
    """A completed agent step and its validated output."""

    model_config = ConfigDict(extra="forbid")

    step: int
    agent: str
    task: str
    attempts: int
    duration_seconds: float
    output: dict[str, Any]


class OrchestrationResult(BaseModel):
    """Result of one Sun run, including partial outputs and structured failures."""

    model_config = ConfigDict(extra="forbid")

    status: Literal[
        "complete",
        "partial",
        "failed",
        "needs_clarification",
        "awaiting_approval",
        "rejected",
    ]
    task: str
    planner: Literal["hardcoded", "llm"]
    plan: list[PlanStep]
    outputs: list[StepResult]
    stopped_at: str | None
    reason: str | None
    failed_at: str | None = None
    failure: Failure | None = None
    warning: str | None = None
    run_id: str | None = None
    duration_seconds: float


class InboundRequest(BaseModel):
    """Request body for POST /inbound (Phase 4 real input)."""

    model_config = ConfigDict(extra="forbid")

    message: str = Field(min_length=1)
    source: str = "unknown"


class InboundResponse(BaseModel):
    """Accepted inbound message; the run continues in the background."""

    model_config = ConfigDict(extra="forbid")

    run_id: str
    status: Literal["accepted"] = "accepted"
