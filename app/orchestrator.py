"""The Sun: plan a task, run agents sequentially, and pass outputs forward.

Phase 5 makes every step bounded and observable: per-call timeouts, retries with
backoff, a whole-run budget, structured failures, per-agent/per-tool SQLite
audit rows, and a human-approval gate before irreversible actions.
"""

from __future__ import annotations

import json
import logging
import time
from typing import Any

from pydantic import BaseModel

from app import events
from app import logger as run_store
from app.agents import (
    AGENT_NAME_RE,
    AGENTS_DIR,
    PLAN_CONTRACT,
    PLANNER_AGENT,
    STEP_CONTRACT,
    AgentNotFoundError,
    AgentRun,
    AgentRunError,
    run_agent,
)
from app.approvals import approval_required, create_pending
from app.config import settings
from app.llm import resolved_model
from app.reliability import Deadline, RunTimeout, redact, truncate
from app.schemas import (
    AgentOutput,
    CheckerOutput,
    Failure,
    FailureType,
    OrchestrationResult,
    PlanStep,
    StepResult,
    SunPlan,
    WriterOutput,
)
from app.toolbox import AGENT_TOOLS, Toolset
from tools.search import SearchUnavailable, search
from tools.telegram import TelegramError, send_message

logger = logging.getLogger("sol_o.sun")

DEFAULT_PLAN = ("researcher", "writer", "checker")

# Code-only planets: appended after every plan, never chosen by the planner.
SYSTEM_STEPS = ("logger", "notifier")
LOGGER_TASK = "Save this run, its draft, verdict and outputs to SQLite."
NOTIFIER_TASK = "Send the task, draft, verdict and issues to the operator on Telegram."

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
    """Single event sink: console log plus the Phase 6 SSE bus."""
    payload = json.dumps(data, ensure_ascii=False, default=str)
    logger.info("event=%s %s", event, redact(payload))
    events.emit(event, **data)


def _summary(agent: str, output: dict[str, Any], limit: int = 140) -> str:
    """One-line human summary for the planet label/tooltip, capped for the UI."""
    for key in ("summary", "draft", "verdict", "status"):
        value = output.get(key)
        if value:
            text = " ".join(str(value).split())
            return text if len(text) <= limit else text[: limit - 1] + "…"
    return f"{agent} finished"


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
    """Hardcoded researcher -> writer -> checker plan (logger/notifier follow)."""
    return [PlanStep(agent=agent, task=task) for agent in DEFAULT_PLAN]


def _elapsed_ms(started: float) -> int:
    return int((time.perf_counter() - started) * 1000)


def _make_failure(
    agent: str,
    error_type: FailureType,
    message: str,
    raw: str = "",
    attempts: int = 0,
) -> Failure:
    return Failure(
        agent=agent,
        error_type=error_type,
        message=truncate(redact(message)),
        raw_output=truncate(redact(raw)),
        attempts=attempts,
    )


def _record_agent_call(**values: Any) -> None:
    try:
        run_store.save_agent_call(**values)
    except Exception:  # audit logging must never take the run down
        logger.exception("could not write agent_calls row agent=%s", values.get("agent"))


def _record_tool_call(**values: Any) -> None:
    try:
        run_store.save_tool_call(**values)
    except Exception:  # audit logging must never take the run down
        logger.exception("could not write tool_calls row tool=%s", values.get("tool"))


def _run_logged_agent(
    *,
    run_id: str,
    step: int,
    agent: str,
    message: str,
    model: type[BaseModel],
    task: str,
    deadline: Deadline,
    contract: str = STEP_CONTRACT,
    response_schema: dict | None = None,
    toolset: Toolset | None = None,
) -> AgentRun:
    """Run one agent and audit it in ``agent_calls``, success or failure."""
    remaining = deadline.remaining()
    if remaining <= 0:
        raise RunTimeout(f"run exceeded {deadline.seconds:.1f}s before agent={agent}")

    model_name = resolved_model(agent)
    started_at = run_store.now_iso()
    started = time.perf_counter()

    def on_attempt(attempt: int) -> None:
        emit("agent_working", step=step, agent=agent, attempt=attempt)

    try:
        run = run_agent(
            agent,
            message,
            model,
            contract=contract,
            task=task,
            response_schema=response_schema,
            timeout_seconds=min(settings.llm_timeout_seconds, remaining),
            on_attempt=on_attempt,
            toolset=toolset,
        )
    except AgentRunError as exc:
        _record_agent_call(
            run_id=run_id,
            step=step,
            agent=agent,
            started_at=started_at,
            duration_ms=_elapsed_ms(started),
            attempts=exc.attempts,
            status="failed",
            error_type=exc.error_type.value,
            error_message=exc.detail,
            input_chars=len(message),
            output_chars=len(exc.raw),
            raw_output=exc.raw,
            model=model_name,
        )
        raise

    raw_output = run.output.model_dump_json()
    _record_agent_call(
        run_id=run_id,
        step=step,
        agent=agent,
        started_at=started_at,
        duration_ms=_elapsed_ms(started),
        attempts=run.attempts,
        status="ok",
        input_chars=len(message),
        output_chars=len(raw_output),
        raw_output=raw_output,
        model=model_name,
    )
    return run


def plan_with_llm(task: str, run_id: str, deadline: Deadline) -> list[PlanStep]:
    """Ask the Sun planner for a plan, then drop unknown or out-of-order agents and cap it."""
    agents = available_agents()
    user_content = f"Task:\n{task.strip()}\n\nAvailable agents:\n" + "\n".join(
        f"- {name}" for name in agents
    )
    # Pydantic validates structure; the provider schema also forces the agent
    # field to a known id and the plan length into 1..MAX_AGENTS.
    response_schema = SunPlan.model_json_schema()
    response_schema["$defs"]["PlanStep"]["properties"]["agent"]["enum"] = agents
    response_schema["properties"]["steps"]["minItems"] = 1
    response_schema["properties"]["steps"]["maxItems"] = settings.max_agents
    run = _run_logged_agent(
        run_id=run_id,
        step=0,
        agent=PLANNER_AGENT,
        message=user_content,
        model=SunPlan,
        task=task,
        deadline=deadline,
        contract=PLAN_CONTRACT,
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
        if len(steps) == settings.max_agents:
            logger.warning("planner exceeded %d steps; truncating", settings.max_agents)
            break
        steps.append(step)
        seen.add(step.agent)
    return steps


def _search_context(task: str, run_id: str, deadline: Deadline) -> dict[str, Any]:
    """Run Tavily once per orchestration; degrade to "no results" instead of failing."""
    query = " ".join(task.split())[:400]
    no_results_note = (
        "No search results are available. Do not invent sources: mark every claim "
        "'unverified' and list what could not be checked in gaps."
    )
    if deadline.expired():
        raise RunTimeout("run exceeded its time budget before search")
    started_at = run_store.now_iso()
    started = time.perf_counter()
    try:
        results = search(query)
    except SearchUnavailable as exc:
        _record_tool_call(
            run_id=run_id,
            tool="search",
            started_at=started_at,
            duration_ms=_elapsed_ms(started),
            status="failed",
            error_message=str(exc),
        )
        emit("search_failed", query=query, reason=str(exc))
        return {"query": query, "results": [], "note": no_results_note}
    except Exception as exc:  # search must never break a run
        logger.exception("unexpected search failure")
        _record_tool_call(
            run_id=run_id,
            tool="search",
            started_at=started_at,
            duration_ms=_elapsed_ms(started),
            status="failed",
            error_message=redact(str(exc)),
        )
        emit("search_failed", query=query, reason=redact(str(exc)))
        return {"query": query, "results": [], "note": no_results_note}

    _record_tool_call(
        run_id=run_id,
        tool="search",
        started_at=started_at,
        duration_ms=_elapsed_ms(started),
        status="ok",
    )
    emit("search_done", query=query, results=len(results))
    note = "" if results else (
        "The search returned no results. Mark every claim 'unverified' and list gaps."
    )
    return {"query": query, "results": results, "note": note}


def _send_telegram(run_id: str, text: str, deadline: Deadline) -> dict[str, object]:
    """Send through the notifier and audit the tool call."""
    if deadline.expired():
        raise RunTimeout("run exceeded its time budget before sending")
    started_at = run_store.now_iso()
    started = time.perf_counter()
    try:
        delivery = send_message(text)
    except TelegramError as exc:
        _record_tool_call(
            run_id=run_id,
            tool="telegram",
            started_at=started_at,
            duration_ms=_elapsed_ms(started),
            status="failed",
            error_message=str(exc),
        )
        raise
    except Exception as exc:  # report, never swallow
        _record_tool_call(
            run_id=run_id,
            tool="telegram",
            started_at=started_at,
            duration_ms=_elapsed_ms(started),
            status="failed",
            error_message=redact(str(exc)),
        )
        raise TelegramError(str(exc)) from exc

    _record_tool_call(
        run_id=run_id,
        tool="telegram",
        started_at=started_at,
        duration_ms=_elapsed_ms(started),
        status="ok",
    )
    return delivery


def _agent_message(
    task: str, step: PlanStep, earlier: list[StepResult], extra: str = ""
) -> str:
    """Original task + this step's instruction + every earlier validated output."""
    prior = [{"agent": item.agent, "output": item.output} for item in earlier]
    message = (
        f"Original task:\n{task}\n\n"
        f"Your step instruction:\n{step.task}\n\n"
        "Outputs from earlier agents (JSON):\n"
        f"{json.dumps(prior, ensure_ascii=False, indent=2)}"
    )
    if extra:
        message = f"{message}\n\n{extra}"
    return message


def _revision_task(issues: list[dict[str, Any]]) -> str:
    payload = json.dumps(issues, ensure_ascii=False, indent=2)
    return (
        "Revise your draft so it resolves every issue the checker reported. Use only facts "
        "from the earlier outputs; do not add new claims. Return the complete revised draft.\n"
        f"Issues:\n{payload}"
    )


REVISION_CHECK_TASK = (
    "Re-review the revised writer draft against the researcher findings and confirm whether "
    "every previously reported issue is fixed."
)


def _final_draft(outputs: list[StepResult]) -> str:
    for item in reversed(outputs):
        if item.agent == "writer" and item.output.get("draft"):
            return str(item.output["draft"])
    return ""


def _final_verdict(outputs: list[StepResult]) -> str | None:
    for item in reversed(outputs):
        if item.agent == "checker" and item.output.get("verdict"):
            return str(item.output["verdict"])
    return None


def _final_issues(outputs: list[StepResult]) -> list[dict[str, Any]]:
    for item in reversed(outputs):
        if item.agent == "checker":
            issues = item.output.get("issues")
            if isinstance(issues, list):
                return [issue for issue in issues if isinstance(issue, dict)]
    return []


def _notification_text(
    *,
    task: str,
    status: str,
    draft: str,
    verdict: str | None,
    issues: list[dict[str, Any]],
    reason: str | None,
) -> str:
    lines = [
        f"Sol-O run: {status}",
        f"Task: {task}",
        f"Checker verdict: {verdict or 'n/a'}",
        "",
        "Draft:",
        draft or "(no draft)",
    ]
    if issues:
        lines += ["", "Issues:"]
        lines += [f"- {issue.get('problem', '')} ({issue.get('location', '')})" for issue in issues]
    if reason and not issues:
        lines += ["", f"Note: {reason}"]
    return redact("\n".join(lines))


def _approval_payload(
    *,
    task: str,
    status: str,
    draft: str,
    verdict: str | None,
    issues: list[dict[str, Any]],
    reason: str | None,
) -> dict[str, Any]:
    return {
        "run_status": status,
        "task": redact(task),
        "draft": redact(draft),
        "verdict": verdict,
        "issues": issues,
        "reason": redact(reason) if reason else None,
        "message": _notification_text(
            task=task,
            status=status,
            draft=draft,
            verdict=verdict,
            issues=issues,
            reason=reason,
        ),
    }


def _system_step(number: int, agent: str, output: dict[str, Any], seconds: float) -> StepResult:
    return StepResult(
        step=number,
        agent=agent,
        task=LOGGER_TASK if agent == "logger" else NOTIFIER_TASK,
        attempts=1,
        duration_seconds=round(seconds, 2),
        output=output,
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
    run_id: str,
    started: float,
    failed_at: str | None,
    failure: Failure | None,
    warning: str | None,
) -> OrchestrationResult:
    duration = round(time.perf_counter() - started, 2)
    if status == "failed":
        emit(
            "run_failed",
            failed_at=failed_at,
            error_type=failure.error_type.value if failure else "llm_error",
            message=reason or "run failed",
        )
    elif status != "awaiting_approval":
        emit(
            "all_done",
            status=status,
            seconds=duration,
            steps=len(outputs),
            duration_seconds=duration,
        )
    logger.info(
        "orchestration id=%s planner=%s status=%s steps=%d/%d stopped_at=%s duration=%.2fs",
        run_id,
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
        failed_at=failed_at,
        failure=failure,
        warning=warning,
        run_id=run_id,
        duration_seconds=duration,
    )


def orchestrate(
    task: str,
    use_llm_planner: bool = False,
    run_id: str | None = None,
    source: str = "",
) -> OrchestrationResult:
    """Run one orchestration with its events bound to the run id for SSE."""
    resolved = run_id or run_store.new_run_id()
    with events.run_context(resolved):
        return _orchestrate(
            task, use_llm_planner=use_llm_planner, run_id=resolved, source=source
        )


def _orchestrate(
    task: str,
    use_llm_planner: bool = False,
    run_id: str | None = None,
    source: str = "",
) -> OrchestrationResult:
    """Run one orchestration: plan, execute sequentially, log, gate, notify."""
    started = time.perf_counter()
    deadline = Deadline(started=started, seconds=settings.run_timeout_seconds)
    run_id = run_id or run_store.new_run_id()
    planner = "llm" if use_llm_planner else "hardcoded"

    warning: str | None = None
    if len(task) > settings.max_input_chars:
        warning = (
            f"task truncated from {len(task)} to {settings.max_input_chars} characters"
        )
        task = task[: settings.max_input_chars]
        logger.warning(warning)

    try:
        run_store.start_run(run_id, task, source=source)
    except Exception as exc:  # logging must never take the run down
        logger.exception("could not create run row")
        emit("logger_failed", phase="start", run_id=run_id, reason=str(exc))

    emit("run_started", task=redact(task))

    outputs: list[StepResult] = []
    llm_steps: list[PlanStep] = []
    status = "failed"
    stopped_at: str | None = None
    reason: str | None = None
    failed_at: str | None = None
    failure: Failure | None = None

    try:
        llm_steps = plan_with_llm(task, run_id, deadline) if use_llm_planner else default_plan(task)
    except RunTimeout as exc:
        status, failed_at, stopped_at = "failed", PLANNER_AGENT, PLANNER_AGENT
        reason = "run_timeout"
        failure = _make_failure(PLANNER_AGENT, FailureType.TIMEOUT, f"run_timeout: {exc}")
        emit("stopped", step=0, agent=PLANNER_AGENT, status=status, reason=reason)
    except AgentRunError as exc:
        status, failed_at, stopped_at = "failed", PLANNER_AGENT, PLANNER_AGENT
        reason = f"{exc.error_type.value}: {exc.detail}"
        failure = _make_failure(
            PLANNER_AGENT, exc.error_type, f"planner failed: {exc.detail}", exc.raw, exc.attempts
        )
        emit("stopped", step=0, agent=PLANNER_AGENT, status=status, reason=reason)
    except AgentNotFoundError as exc:
        status, failed_at, stopped_at = "failed", PLANNER_AGENT, PLANNER_AGENT
        reason = f"llm_error: planner prompt missing ({exc.agent})"
        failure = _make_failure(PLANNER_AGENT, FailureType.LLM_ERROR, reason, attempts=1)
        emit("stopped", step=0, agent=PLANNER_AGENT, status=status, reason=reason)

    if not reason and not llm_steps:
        status, failed_at, stopped_at = "failed", PLANNER_AGENT, PLANNER_AGENT
        reason = "planner returned no usable steps"
        failure = _make_failure(PLANNER_AGENT, FailureType.INVALID_JSON, reason, attempts=1)
        emit("stopped", step=0, agent=PLANNER_AGENT, status=status, reason=reason)

    if not reason:
        emit(
            "plan_ready",
            planner=planner,
            agents=[step.agent for step in llm_steps],
            steps=[step.model_dump() for step in llm_steps],
        )

    search_block: dict[str, Any] | None = None
    revision_used = False
    index = 0
    status = "failed" if reason else "complete"

    while not reason and index < len(llm_steps):
        step = llm_steps[index]
        model = AGENT_OUTPUT_MODELS[step.agent]
        number = len(outputs) + 1
        emit("agent_called", step=number, agent=step.agent)

        extra = ""
        if step.agent == "researcher":
            if search_block is None:
                emit("agent_working", step=number, agent=step.agent, attempt=1)
                try:
                    search_block = _search_context(task, run_id, deadline)
                except RunTimeout as exc:
                    status, failed_at, stopped_at = "failed", step.agent, step.agent
                    reason = "run_timeout"
                    failure = _make_failure(step.agent, FailureType.TIMEOUT, f"run_timeout: {exc}")
                    emit("stopped", step=number, agent=step.agent, status=status, reason=reason)
                    break
            extra = "Web search results (JSON):\n" + json.dumps(
                search_block, ensure_ascii=False, indent=2
            )

        try:
            run = _run_logged_agent(
                run_id=run_id,
                step=number,
                agent=step.agent,
                message=_agent_message(task, step, outputs, extra),
                model=model,
                task=task,
                deadline=deadline,
                toolset=(
                    Toolset(
                        agent=step.agent,
                        step=number,
                        run_id=run_id,
                        source=source,
                        deadline=deadline,
                        emit=emit,
                    )
                    if step.agent in AGENT_TOOLS
                    else None
                ),
            )
        except RunTimeout as exc:
            status, failed_at, stopped_at = "failed", step.agent, step.agent
            reason = "run_timeout"
            failure = _make_failure(step.agent, FailureType.TIMEOUT, f"run_timeout: {exc}")
            emit(
                "agent_failed",
                step=number,
                agent=step.agent,
                error="timeout",
                error_type="timeout",
                message=str(exc),
                attempts=0,
            )
            emit("stopped", step=number, agent=step.agent, status=status, reason=reason)
            break
        except AgentRunError as exc:
            run_timed_out = deadline.expired()
            error_type = FailureType.TIMEOUT if run_timed_out else exc.error_type
            status, failed_at, stopped_at = "failed", step.agent, step.agent
            reason = "run_timeout" if run_timed_out else f"{exc.error_type.value}: {exc.detail}"
            failure = _make_failure(
                step.agent, error_type, exc.detail, exc.raw, exc.attempts
            )
            emit(
                "agent_failed",
                step=number,
                agent=step.agent,
                error=error_type.value,
                error_type=error_type.value,
                message=exc.detail,
                attempts=exc.attempts,
            )
            emit("stopped", step=number, agent=step.agent, status=status, reason=reason)
            break

        output = run.output
        step_duration = round(run.duration_seconds, 2)
        outputs.append(
            StepResult(
                step=number,
                agent=step.agent,
                task=step.task,
                attempts=run.attempts,
                duration_seconds=step_duration,
                output=output.model_dump(mode="json"),
            )
        )
        emit(
            "agent_done",
            step=number,
            agent=step.agent,
            status=output.status,
            attempts=run.attempts,
            duration_seconds=step_duration,
            summary=_summary(step.agent, output.model_dump(mode="json")),
        )

        if output.status == "needs_clarification":
            status = "needs_clarification"
            stopped_at = step.agent
            reason = output.clarification_needed or "agent requested clarification"
            emit("stopped", step=number, agent=step.agent, status=status, reason=reason)
            break

        if step.agent == "checker" and getattr(output, "verdict", None) == "needs_changes":
            issues = [
                issue.model_dump() if hasattr(issue, "model_dump") else issue
                for issue in getattr(output, "issues", [])
            ]
            if revision_used:
                status = "partial"
                stopped_at = "checker"
                reason = (
                    f"checker still rejected the revised draft with {len(issues)} issue(s)"
                )
                emit(
                    "stopped",
                    step=number,
                    agent=step.agent,
                    status=status,
                    reason=reason,
                )
                break
            revision_used = True
            emit("revision_requested", step=number, issues=len(issues))
            llm_steps[index + 1 : index + 1] = [
                PlanStep(agent="writer", task=_revision_task(issues)),
                PlanStep(agent="checker", task=REVISION_CHECK_TASK),
            ]

        index += 1

    plan = list(llm_steps) + [
        PlanStep(agent=agent, task=LOGGER_TASK if agent == "logger" else NOTIFIER_TASK)
        for agent in SYSTEM_STEPS
    ]

    final_draft = _final_draft(outputs)
    final_verdict = _final_verdict(outputs)
    final_issues = _final_issues(outputs)

    # Decide the notifier path before the logger row is written so its status is accurate.
    notify_mode = "send"
    pending_action_id: int | None = None
    notify_error: str | None = None
    approval_payload: dict[str, Any] | None = None

    if status == "failed":
        notify_mode = "skipped"
    elif approval_required():
        try:
            approval_payload = _approval_payload(
                task=task,
                status=status,
                draft=final_draft,
                verdict=final_verdict,
                issues=final_issues,
                reason=reason,
            )
            pending_action_id = create_pending(run_id, approval_payload)
            notify_mode = "pending"
            status = "awaiting_approval"
            if stopped_at is None:
                stopped_at = "notifier"
            if reason is None:
                reason = f"awaiting human approval for pending action {pending_action_id}"
            emit(
                "approval_required",
                run_id=run_id,
                action_id=pending_action_id,
                status=status,
                payload={
                    "task": approval_payload["task"],
                    "draft": approval_payload["draft"],
                    "verdict": final_verdict,
                    "issues": final_issues,
                },
            )
            emit(
                "awaiting_approval",
                pending_id=pending_action_id,
                preview=" ".join(str(approval_payload["message"]).split())[:140],
            )
        except Exception as exc:  # never send without a stored approval
            logger.exception("could not store pending action")
            notify_mode = "error"
            notify_error = redact(str(exc))
            status, failed_at, stopped_at = "failed", "notifier", "notifier"
            reason = f"tool_error: {notify_error}"
            failure = _make_failure("notifier", FailureType.TOOL_ERROR, reason)
            emit("notifier_failed", agent="notifier", reason=notify_error)

    # Logger: always runs, success or failure.
    number = len(outputs) + 1
    emit("agent_called", step=number, agent="logger")
    step_started = time.perf_counter()
    try:
        run_store.save_run(
            run_id,
            task=task,
            status=status,
            final_draft=final_draft,
            verdict=final_verdict,
            outputs=[item.model_dump(mode="json") for item in outputs],
            duration_seconds=time.perf_counter() - started,
            warning=warning,
        )
        log_output: dict[str, Any] = {
            "agent": "logger",
            "status": "complete",
            "run_id": run_id,
            "database": str(run_store.db_path()),
        }
        emit("logger_saved", step=number, agent="logger", run_id=run_id)
    except Exception as exc:  # logging must never take the run down
        logger.exception("could not save run")
        log_output = {"agent": "logger", "status": "failed", "run_id": run_id, "error": str(exc)}
        emit("logger_failed", phase="save", run_id=run_id, reason=str(exc))
    outputs.append(_system_step(number, "logger", log_output, time.perf_counter() - step_started))
    emit(
        "agent_done",
        step=number,
        agent="logger",
        status=log_output["status"],
        attempts=1,
        summary=_summary("logger", log_output),
    )

    # Notifier: executes, waits for approval, or skips failed runs.
    number = len(outputs) + 1
    emit("agent_called", step=number, agent="notifier")
    step_started = time.perf_counter()
    if notify_mode == "pending":
        notify_output: dict[str, Any] = {
            "agent": "notifier",
            "status": "awaiting_approval",
            "pending_action_id": pending_action_id,
        }
        emit("notifier_pending", step=number, action_id=pending_action_id)
    elif notify_mode == "skipped":
        notify_output = {
            "agent": "notifier",
            "status": "skipped",
            "reason": "run failed; nothing sent",
        }
        emit("notifier_skipped", step=number)
    elif notify_mode == "error":
        notify_output = {
            "agent": "notifier",
            "status": "failed",
            "error": notify_error or "could not store pending action",
        }
    else:
        text = _notification_text(
            task=task,
            status=status,
            draft=final_draft,
            verdict=final_verdict,
            issues=final_issues,
            reason=reason,
        )
        try:
            emit("agent_working", step=number, agent="notifier", attempt=1)
            delivery = _send_telegram(run_id, text, deadline)
            notify_output = {"agent": "notifier", "status": "complete", **delivery}
            emit("notifier_sent", step=number, **delivery)
        except (TelegramError, RunTimeout) as exc:
            timeout = isinstance(exc, RunTimeout)
            notify_output = {
                "agent": "notifier",
                "status": "failed",
                "error": redact(str(exc)),
            }
            if status != "failed":
                status, failed_at, stopped_at = "failed", "notifier", "notifier"
                reason = "run_timeout" if timeout else f"tool_error: {redact(str(exc))}"
                failure = _make_failure(
                    "notifier",
                    FailureType.TIMEOUT if timeout else FailureType.TOOL_ERROR,
                    str(exc),
                )
                try:
                    run_store.set_run_status(run_id, status)
                except Exception:  # reported below, never silent
                    logger.exception("could not update failed run status")
            emit("notifier_failed", step=number, reason=redact(str(exc)))
    outputs.append(_system_step(number, "notifier", notify_output, time.perf_counter() - step_started))
    emit(
        "agent_done",
        step=number,
        agent="notifier",
        status=notify_output["status"],
        attempts=1,
        summary=_summary("notifier", notify_output),
    )

    return _finish(
        status=status,
        task=task,
        planner=planner,
        plan=plan,
        outputs=outputs,
        stopped_at=stopped_at,
        reason=reason,
        run_id=run_id,
        started=started,
        failed_at=failed_at,
        failure=failure,
        warning=warning,
    )
