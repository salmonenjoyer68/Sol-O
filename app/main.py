"""Sol-O Phase 1 API: run one file-backed agent and return validated JSON."""

from __future__ import annotations

import json
import logging
import re
import time
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from pydantic import ValidationError

from app.llm import LLMError, LLMTimeoutError, call_llm
from app.schemas import AgentOutput, RunRequest

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)
logger = logging.getLogger("sol_o")

AGENTS_DIR = Path(__file__).resolve().parent.parent / "agents"
AGENT_NAME_RE = re.compile(r"^[A-Za-z0-9_-]+$")
MAX_ATTEMPTS = 3  # initial call + 2 retries
FENCE_RE = re.compile(r"^\s*```(?:json)?\s*(.*?)\s*```\s*$", re.DOTALL | re.IGNORECASE)

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

RETRY_INSTRUCTION = """Your previous response could not be used: {error}

Return the corrected, complete JSON object only. No prose and no markdown fences."""

app = FastAPI(title="Sol-O Phase 1")


class ApiError(Exception):
    """Raised to return an exact JSON error body and status code."""

    def __init__(self, status_code: int, content: dict) -> None:
        self.status_code = status_code
        self.content = content


@app.exception_handler(ApiError)
async def api_error_handler(_request: Request, exc: ApiError) -> JSONResponse:
    return JSONResponse(status_code=exc.status_code, content=exc.content)


def load_system_prompt(agent: str) -> str:
    """Read agents/{agent}.md on every request, or raise 404."""
    if not AGENT_NAME_RE.fullmatch(agent):
        raise ApiError(404, {"error": "agent_not_found", "agent": agent})
    path = AGENTS_DIR / f"{agent}.md"
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise ApiError(404, {"error": "agent_not_found", "agent": agent}) from None
    except OSError as exc:
        raise ApiError(500, {"error": "agent_unreadable", "detail": str(exc)}) from exc


def parse_agent_output(raw: str) -> AgentOutput:
    """Strip optional markdown fences, parse JSON, and validate the contract."""
    text = raw.strip()
    fenced = FENCE_RE.match(text)
    if fenced:
        text = fenced.group(1).strip()
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise ValueError(f"invalid JSON: {exc.msg} (line {exc.lineno}, column {exc.colno})") from exc
    try:
        return AgentOutput.model_validate(data)
    except ValidationError as exc:
        raise ValueError(f"schema validation failed: {exc}") from exc


def _short(text: str, limit: int = 200) -> str:
    collapsed = " ".join(text.split())
    return collapsed if len(collapsed) <= limit else collapsed[:limit] + "..."


def _log(level: int, agent: str, task: str, attempts: int, started: float, result: str) -> None:
    logger.log(
        level,
        "agent=%s task=%r attempts=%d duration=%.2fs result=%s",
        agent,
        _short(task),
        attempts,
        time.perf_counter() - started,
        result,
    )


@app.post("/run", response_model=AgentOutput)
def run(request: RunRequest) -> AgentOutput:
    started = time.perf_counter()
    system_prompt = load_system_prompt(request.agent)
    messages: list[dict[str, str]] = [
        {"role": "system", "content": system_prompt + OUTPUT_CONTRACT},
        {"role": "user", "content": request.task},
    ]

    for attempt in range(1, MAX_ATTEMPTS + 1):
        try:
            raw = call_llm(messages)
        except LLMTimeoutError as exc:
            _log(logging.ERROR, request.agent, request.task, attempt, started, "timeout")
            raise ApiError(504, {"error": "llm_timeout", "detail": str(exc)}) from exc
        except LLMError as exc:
            _log(logging.ERROR, request.agent, request.task, attempt, started, "llm_error")
            raise ApiError(502, {"error": "llm_error", "detail": str(exc)}) from exc

        try:
            output = parse_agent_output(raw)
        except ValueError as exc:
            logger.warning(
                "agent=%s attempt=%d/%d invalid output: %s",
                request.agent,
                attempt,
                MAX_ATTEMPTS,
                _short(str(exc), 500),
            )
            if attempt == MAX_ATTEMPTS:
                _log(
                    logging.ERROR,
                    request.agent,
                    request.task,
                    attempt,
                    started,
                    "invalid_agent_output",
                )
                raise ApiError(
                    502, {"error": "invalid_agent_output", "raw": raw}
                ) from None
            messages.append({"role": "assistant", "content": raw})
            messages.append(
                {"role": "user", "content": RETRY_INSTRUCTION.format(error=exc)}
            )
            continue

        _log(logging.INFO, request.agent, request.task, attempt, started, "ok")
        return output

    # The loop always returns or raises on its final iteration.
    raise AssertionError("unreachable")
