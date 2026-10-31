"""Sol-O API: Phase 1 ``/run`` and Phase 2 ``/orchestrate``."""

from __future__ import annotations

import logging

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

from app.agents import OUTPUT_CONTRACT, AgentNotFoundError, AgentRunError, run_agent
from app.orchestrator import orchestrate as run_orchestration
from app.schemas import (
    AgentOutput,
    OrchestrationRequest,
    OrchestrationResult,
    RunRequest,
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)

app = FastAPI(title="Sol-O")


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
        if exc.kind == "llm_timeout":
            raise ApiError(504, {"error": "llm_timeout", "detail": exc.detail}) from exc
        if exc.kind == "llm_error":
            raise ApiError(502, {"error": "llm_error", "detail": exc.detail}) from exc
        raise ApiError(502, {"error": "invalid_agent_output", "raw": exc.raw}) from exc

    return result.output


@app.post("/orchestrate", response_model=OrchestrationResult)
def orchestrate(request: OrchestrationRequest) -> OrchestrationResult:
    """Plan a task, run agents sequentially, and return the compiled result."""
    return run_orchestration(request.task, use_llm_planner=request.use_llm_planner)
