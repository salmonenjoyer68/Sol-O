"""Pydantic contracts for the Phase 1 agent API."""

from __future__ import annotations

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


class OrchestrationRequest(BaseModel):
    """Request body for POST /orchestrate."""

    model_config = ConfigDict(extra="forbid")

    task: str = Field(min_length=1)
    use_llm_planner: bool = False


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
    """Result of one Sun run, including partial outputs when it stopped early."""

    model_config = ConfigDict(extra="forbid")

    status: Literal["complete", "failed", "needs_clarification"]
    task: str
    planner: Literal["hardcoded", "llm"]
    plan: list[PlanStep]
    outputs: list[StepResult]
    stopped_at: str | None
    reason: str | None
    duration_seconds: float
