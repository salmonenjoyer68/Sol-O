"""Pydantic contracts for the Phase 1 agent API."""

from __future__ import annotations

from typing import Literal

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
