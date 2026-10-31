"""Sol-O API: agents, orchestration, run history, inbound input and approvals."""

from __future__ import annotations

import asyncio
import json
import logging
from pathlib import Path
from typing import Any

from fastapi import BackgroundTasks, FastAPI, Query, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import HTMLResponse, JSONResponse, StreamingResponse

from app import approvals, events
from app import logger as run_store
from app.agents import OUTPUT_CONTRACT, AgentNotFoundError, AgentRunError, run_agent
from app.orchestrator import orchestrate as run_orchestration
from app.reliability import install_redaction_filter, redact
from app.schemas import (
    AgentOutput,
    FailureType,
    InboundRequest,
    InboundResponse,
    OrchestrationRequest,
    RunRequest,
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)
install_redaction_filter()

api_logger = logging.getLogger("sol_o.api")

ROOT = Path(__file__).resolve().parent.parent
WEB_INDEX = ROOT / "web" / "index.html"
SSE_KEEPALIVE_SECONDS = 15.0

app = FastAPI(title="Sol-O")

# The browser UI is same-origin; CORS still allows local tooling on localhost only.
app.add_middleware(
    CORSMiddleware,
    allow_origin_regex=r"^https?://(localhost|127\.0\.0\.1)(:\d+)?$",
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


class ApiError(Exception):
    """Raised to return an exact JSON error body and status code."""

    def __init__(self, status_code: int, content: dict) -> None:
        self.status_code = status_code
        self.content = content


@app.exception_handler(ApiError)
async def api_error_handler(_request: Request, exc: ApiError) -> JSONResponse:
    return JSONResponse(status_code=exc.status_code, content=exc.content)


@app.post("/run", response_model=AgentOutput)
def run(request: RunRequest) -> AgentOutput:
    """Run one file-backed agent and return its validated JSON."""
    try:
        result = run_agent(
            request.agent,
            request.task,
            AgentOutput,
            contract=OUTPUT_CONTRACT,
            task=request.task,
        )
    except AgentNotFoundError as exc:
        raise ApiError(404, {"error": "agent_not_found", "agent": exc.agent}) from exc
    except OSError as exc:
        raise ApiError(500, {"error": "agent_unreadable", "detail": str(exc)}) from exc
    except AgentRunError as exc:
        if exc.error_type == FailureType.TIMEOUT:
            raise ApiError(504, {"error": "llm_timeout", "detail": exc.detail}) from exc
        if exc.error_type == FailureType.LLM_ERROR:
            raise ApiError(502, {"error": "llm_error", "detail": exc.detail}) from exc
        raise ApiError(
            502, {"error": "invalid_agent_output", "raw": redact(exc.raw)}
        ) from exc

    return result.output


def _fail_run(run_id: str, exc: BaseException) -> None:
    """Record an unexpected background failure so the live stream always terminates."""
    try:
        run_store.set_run_status(run_id, "failed")
    except Exception:
        api_logger.exception("could not mark run failed run_id=%s", run_id)
    try:
        events.publish_run(
            run_id,
            "run_failed",
            {
                "failed_at": "sun",
                "error_type": "llm_error",
                "message": redact(str(exc)),
            },
        )
    except Exception:
        api_logger.exception("could not publish run_failed run_id=%s", run_id)


def _run_orchestration(task: str, use_llm_planner: bool, run_id: str) -> None:
    api_logger.info("orchestration queued run_id=%s", run_id)
    try:
        run_orchestration(task, use_llm_planner=use_llm_planner, run_id=run_id)
    except Exception as exc:  # a background failure must not kill the worker
        api_logger.exception("orchestration failed run_id=%s", run_id)
        _fail_run(run_id, exc)


@app.post("/orchestrate")
def orchestrate(request: OrchestrationRequest, background: BackgroundTasks) -> dict[str, str]:
    """Queue a run in the background and return its id so the browser can watch live."""
    run_id = run_store.new_run_id()
    try:
        run_store.start_run(run_id, request.task)
    except Exception:  # still accept the run; the Sun retries the insert
        api_logger.exception("could not create orchestration run row run_id=%s", run_id)
    background.add_task(_run_orchestration, request.task, request.use_llm_planner, run_id)
    return {"run_id": run_id, "status": "accepted"}


@app.get("/runs")
def runs(
    limit: int = Query(20, ge=1, le=100), status: str | None = Query(None)
) -> list[dict[str, Any]]:
    """Return the most recent runs, newest first, optionally filtered by status."""
    return run_store.recent_runs(limit, status)


@app.get("/runs/{run_id}/events")
def run_events(run_id: str) -> list[dict[str, Any]]:
    """Saved events for one run, for browser replay."""
    if run_store.get_run(run_id) is None:
        raise ApiError(404, {"error": "run_not_found", "run_id": run_id})
    return run_store.get_run_events(run_id)


@app.get("/runs/{run_id}")
def run_detail(run_id: str) -> dict[str, Any]:
    """Return one run plus its agent calls, tool calls and pending actions."""
    detail = run_store.get_run_detail(run_id)
    if detail is None:
        raise ApiError(404, {"error": "run_not_found", "run_id": run_id})
    return detail


async def _event_stream(request: Request):
    """SSE generator: backlog after Last-Event-ID, then live events and keepalives."""
    try:
        last_event_id = int(request.headers.get("last-event-id", "0") or 0)
    except ValueError:
        last_event_id = 0

    subscription = events.bus.subscribe(asyncio.get_running_loop(), last_event_id)
    try:
        while True:
            if await request.is_disconnected():
                break
            try:
                record = await asyncio.wait_for(
                    subscription.queue.get(), timeout=SSE_KEEPALIVE_SECONDS
                )
            except asyncio.TimeoutError:
                yield ": keepalive\n\n"
                continue
            data = json.dumps(record, ensure_ascii=False, default=str)
            yield f"id: {record['id']}\ndata: {data}\n\n"
    finally:
        events.bus.unsubscribe(subscription.id)


@app.get("/stream")
async def stream(request: Request) -> StreamingResponse:
    """Live stream of every run event for all connected tabs."""
    return StreamingResponse(
        _event_stream(request),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


@app.get("/", response_class=HTMLResponse)
def index() -> HTMLResponse:
    """Serve the self-contained solar-system UI."""
    return HTMLResponse(WEB_INDEX.read_text(encoding="utf-8"))


def _approval_error(exc: approvals.ApprovalError) -> ApiError:
    return ApiError(exc.status_code, {"error": exc.code, "detail": redact(str(exc))})


@app.get("/pending")
def pending_actions() -> list[dict[str, Any]]:
    """List pending actions that are still inside the approval TTL."""
    return approvals.list_pending()


@app.post("/pending/{action_id}/approve")
def approve_pending(action_id: int) -> dict[str, Any]:
    """Execute the stored payload once; a second approve fails."""
    try:
        return approvals.approve(action_id)
    except approvals.ApprovalError as exc:
        raise _approval_error(exc) from exc


@app.post("/pending/{action_id}/reject")
def reject_pending(action_id: int) -> dict[str, Any]:
    """Reject a pending action and mark its run rejected; nothing executes."""
    try:
        return approvals.reject(action_id)
    except approvals.ApprovalError as exc:
        raise _approval_error(exc) from exc


def _run_inbound(message: str, source: str, run_id: str) -> None:
    api_logger.info("inbound source=%s run_id=%s", source, run_id)
    try:
        run_orchestration(message, run_id=run_id)
    except Exception as exc:  # a background failure must not kill the worker
        api_logger.exception("inbound run failed run_id=%s", run_id)
        _fail_run(run_id, exc)


@app.post("/inbound", response_model=InboundResponse, status_code=202)
def inbound(request: InboundRequest, background: BackgroundTasks) -> InboundResponse:
    """Accept a message, queue the full orchestration, return the run id immediately."""
    run_id = run_store.new_run_id()
    try:
        run_store.start_run(run_id, request.message)
    except Exception:  # still accept the message; the Sun retries the insert
        api_logger.exception("could not create inbound run row run_id=%s", run_id)
    background.add_task(_run_inbound, request.message, request.source, run_id)
    return InboundResponse(run_id=run_id)
