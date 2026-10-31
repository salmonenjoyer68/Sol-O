"""The Sun: plan a task, run agents sequentially, and pass outputs forward."""

from __future__ import annotations

import json
import logging
import time
from typing import Any

from pydantic import BaseModel

from app.agents import (
    AGENT_NAME_RE,
    AGENTS_DIR,
    PLAN_CONTRACT,
    PLANNER_AGENT,
    AgentNotFoundError,
    AgentRunError,
    run_agent,
)
from app.schemas import (
    AgentOutput,
    CheckerOutput,
    OrchestrationResult,
    PlanStep,
    StepResult,
    SunPlan,
    WriterOutput,
)

logger = logging.getLogger("sol_o.sun")

MAX_STEPS = 7
DEFAULT_PLAN = ("researcher", "writer", "checker")

# A step may only run when the agents it consumes have already produced output.
REQUIRED_PRIOR_AGENTS: dict[str, set[str]] = {
    "checker": {"writer"},
}

# Every runnable planet needs both a prompt file and a validation model.
AGENT_OUTPUT_MODELS: dict[str, type[BaseModel]] = {
    "researcher": AgentOutput,
    "writer": WriterOutput,
    "checker": CheckerOutput,
}


def emit(event: str, **data: Any) -> None:
    """Single event sink for the whole run.

    Phase 6 replaces the print with SSE/WebSocket; keep all event output here.
    """
    print(f"[{event}] {json.dumps(data, ensure_ascii=False)}", flush=True)


def available_agents() -> list[str]:
    """Agents the planner may choose: prompt files with a validation model, minus Sun."""
    names: list[str] = []
    for path in sorted(AGENTS_DIR.glob("*.md")):
        name = path.stem
        if name == PLANNER_AGENT or name not in AGENT_OUTPUT_MODELS:
            continue
        if AGENT_NAME_RE.fullmatch(name):
            names.append(name)
    return names


def default_plan(task: str) -> list[PlanStep]:
    """Hardcoded researcher -> writer -> checker plan."""
    return [PlanStep(agent=agent, task=task) for agent in DEFAULT_PLAN]


def plan_with_llm(task: str) -> list[PlanStep]:
    """Ask the Sun planner for a plan, then drop unknown or out-of-order agents and cap it."""
    agents = available_agents()
    user_content = f"Task:\n{task.strip()}\n\nAvailable agents:\n" + "\n".join(
        f"- {name}" for name in agents
    )
    # Pydantic validates structure; the provider schema also forces the agent
    # field to a known id and the plan length into 1..MAX_STEPS.
    response_schema = SunPlan.model_json_schema()
    response_schema["$defs"]["PlanStep"]["properties"]["agent"]["enum"] = agents
    response_schema["properties"]["steps"]["minItems"] = 1
    response_schema["properties"]["steps"]["maxItems"] = MAX_STEPS
    run = run_agent(
        PLANNER_AGENT,
        user_content,
        SunPlan,
        contract=PLAN_CONTRACT,
        task=task,
        response_schema=response_schema,
    )
    plan: SunPlan = run.output
    known = set(agents)
    seen: set[str] = set()
    steps: list[PlanStep] = []
    for step in plan.steps:
        if step.agent not in known:
            logger.warning("planner dropped unknown agent=%s", step.agent)
            continue
        missing = REQUIRED_PRIOR_AGENTS.get(step.agent, set()) - seen
        if missing:
            logger.warning(
                "planner dropped agent=%s missing earlier output from %s",
                step.agent,
                ", ".join(sorted(missing)),
            )
            continue
        if len(steps) == MAX_STEPS:
            logger.warning("planner exceeded %d steps; truncating", MAX_STEPS)
            break
        steps.append(step)
        seen.add(step.agent)
    return steps


def _agent_message(task: str, step: PlanStep, earlier: list[StepResult]) -> str:
    """Original task + this step's instruction + every earlier validated output."""
    prior = [{"agent": item.agent, "output": item.output} for item in earlier]
    return (
        f"Original task:\n{task}\n\n"
        f"Your step instruction:\n{step.task}\n\n"
        "Outputs from earlier agents (JSON):\n"
        f"{json.dumps(prior, ensure_ascii=False, indent=2)}"
    )


def _finish(
    *,
    status: str,
    task: str,
    planner: str,
    plan: list[PlanStep],
    outputs: list[StepResult],
    stopped_at: str | None,
    reason: str | None,
    started: float,
) -> OrchestrationResult:
    duration = round(time.perf_counter() - started, 2)
    if status == "complete":
        emit("all_done", status=status, steps=len(outputs), duration_seconds=duration)
    logger.info(
        "orchestration planner=%s status=%s steps=%d/%d stopped_at=%s duration=%.2fs",
        planner,
        status,
        len(outputs),
        len(plan),
        stopped_at,
        duration,
    )
    return OrchestrationResult(
        status=status,
        task=task,
        planner=planner,
        plan=plan,
        outputs=outputs,
        stopped_at=stopped_at,
        reason=reason,
        duration_seconds=duration,
    )


def orchestrate(task: str, use_llm_planner: bool = False) -> OrchestrationResult:
    """Run one orchestration: plan, execute sequentially, stop on clarification/failure."""
    started = time.perf_counter()
    planner = "llm" if use_llm_planner else "hardcoded"

    try:
        plan = plan_with_llm(task) if use_llm_planner else default_plan(task)
    except (AgentRunError, AgentNotFoundError) as exc:
        kind = getattr(exc, "kind", "agent_not_found")
        reason = f"planner failed ({kind}): {exc}"
        emit("stopped", step=0, agent=PLANNER_AGENT, status="failed", reason=reason)
        return _finish(
            status="failed",
            task=task,
            planner=planner,
            plan=[],
            outputs=[],
            stopped_at=PLANNER_AGENT,
            reason=reason,
            started=started,
        )

    if not plan:
        reason = "planner returned no usable steps"
        emit("stopped", step=0, agent=PLANNER_AGENT, status="failed", reason=reason)
        return _finish(
            status="failed",
            task=task,
            planner=planner,
            plan=[],
            outputs=[],
            stopped_at=PLANNER_AGENT,
            reason=reason,
            started=started,
        )

    emit("plan_ready", planner=planner, steps=[step.model_dump() for step in plan])

    outputs: list[StepResult] = []
    status = "complete"
    stopped_at: str | None = None
    reason: str | None = None

    for step_number, step in enumerate(plan, start=1):
        model = AGENT_OUTPUT_MODELS[step.agent]
        emit("agent_called", step=step_number, agent=step.agent)

        try:
            run = run_agent(
                step.agent,
                _agent_message(task, step, outputs),
                model,
                task=task,
            )
        except AgentRunError as exc:
            status = "failed"
            stopped_at = step.agent
            reason = f"{exc.kind}: {exc.detail}"
            emit(
                "agent_failed",
                step=step_number,
                agent=step.agent,
                error=exc.kind,
                attempts=exc.attempts,
            )
            emit(
                "stopped",
                step=step_number,
                agent=step.agent,
                status=status,
                reason=reason,
            )
            break

        output = run.output
        step_duration = round(run.duration_seconds, 2)
        outputs.append(
            StepResult(
                step=step_number,
                agent=step.agent,
                task=step.task,
                attempts=run.attempts,
                duration_seconds=step_duration,
                output=output.model_dump(mode="json"),
            )
        )
        emit(
            "agent_done",
            step=step_number,
            agent=step.agent,
            status=output.status,
            attempts=run.attempts,
            duration_seconds=step_duration,
        )

        if output.status == "needs_clarification":
            status = "needs_clarification"
            stopped_at = step.agent
            reason = output.clarification_needed or "agent requested clarification"
            emit(
                "stopped",
                step=step_number,
                agent=step.agent,
                status=status,
                reason=reason,
            )
            break

    return _finish(
        status=status,
        task=task,
        planner=planner,
        plan=plan,
        outputs=outputs,
        stopped_at=stopped_at,
        reason=reason,
        started=started,
    )
