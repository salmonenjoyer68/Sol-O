"""Tool registry and the Sun-side executor for planet-requested tools.

A planet asks for tools inside its JSON output (see :mod:`app.agents`); the Sun
validates the request against this registry's allowlist, runs the handler, and
audits every call in ``tool_calls``. Handlers are plain functions in
:mod:`tools`; this module adds naming, allowlists, argument checking, budget
enforcement and the prompt contract that tells a planet what it may call.
"""

from __future__ import annotations

import json
import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

from app import logger as run_store
from app.reliability import Deadline, redact
from app.schemas import ToolRequest
from tools.calc import calculate
from tools.fetch import fetch_url
from tools.kb import search_kb
from tools.recall import recall

logger = logging.getLogger("sol_o.sun")

MAX_TOOL_TURNS = 3
MAX_RESULT_CHARS = 6000
MAX_ARGS_CHARS = 500

TOOL_CONTRACT_TEMPLATE = """
## Tools you may call

You may answer directly, or ask the Sun to run a tool first. To ask, return exactly this object and nothing else:
{{"tool_requests": [{{"tool": "<name>", "args": {{...}}}}]}}

Available tools:
{tools}

Rules: at most {turns} tool turns per step. After that you must return your normal output object. Never request a tool that is not listed above. The envelope must contain only the tool_requests key.
"""


@dataclass(frozen=True)
class ToolContext:
    """Per-step context a handler may need (memory scoping, budget)."""

    run_id: str
    source: str
    agent: str
    step: int
    deadline: Deadline


@dataclass(frozen=True)
class Tool:
    """One callable tool with the one-line signature shown to the model."""

    name: str
    signature: str
    description: str
    required: tuple[str, ...]
    handler: Callable[[dict[str, Any], ToolContext], Any]


def _handler_search(args: dict[str, Any], _ctx: ToolContext) -> Any:
    return search_kb(str(args["query"]), limit=int(args.get("limit", 5)))


def _handler_fetch(args: dict[str, Any], _ctx: ToolContext) -> Any:
    return fetch_url(str(args["url"]))


def _handler_calc(args: dict[str, Any], _ctx: ToolContext) -> Any:
    return calculate(str(args["expression"]))


def _handler_recall(args: dict[str, Any], ctx: ToolContext) -> Any:
    return recall(
        str(args["query"]),
        source=ctx.source,
        run_id=ctx.run_id,
        limit=int(args.get("limit", 5)),
    )


TOOLS: dict[str, Tool] = {
    "kb_search": Tool(
        name="kb_search",
        signature='{"query": str, "limit": int=5}',
        description=(
            "Search the company knowledge base; returns matching chunks with their file path."
        ),
        required=("query",),
        handler=_handler_search,
    ),
    "fetch_url": Tool(
        name="fetch_url",
        signature='{"url": str}',
        description=(
            "Download one http(s) page and return its visible text (first 6000 characters)."
        ),
        required=("url",),
        handler=_handler_fetch,
    ),
    "calc": Tool(
        name="calc",
        signature='{"expression": str}',
        description=(
            'Evaluate arithmetic and date arithmetic, e.g. "3 * 19.99" or '
            "\"days_between('2026-11-01','2026-12-01')\"."
        ),
        required=("expression",),
        handler=_handler_calc,
    ),
    "recall": Tool(
        name="recall",
        signature='{"query": str, "limit": int=5}',
        description="Search earlier runs from this same conversation.",
        required=("query",),
        handler=_handler_recall,
    ),
}

AGENT_TOOLS: dict[str, tuple[str, ...]] = {
    "researcher": ("kb_search", "fetch_url", "recall"),
    "writer": ("kb_search", "calc", "recall"),
    "checker": ("kb_search", "calc"),
}


def _fit(payload: dict[str, Any]) -> dict[str, Any]:
    """Cap the serialized result so one page cannot flood the next prompt."""
    if len(json.dumps(payload, ensure_ascii=False, default=str)) <= MAX_RESULT_CHARS:
        return payload
    return {"tool": payload["tool"], "ok": payload["ok"], "result": "…truncated…"}


@dataclass
class Toolset:
    """The tools one agent step may use, plus the audit and event sinks."""

    agent: str
    step: int
    run_id: str
    source: str
    deadline: Deadline
    emit: Callable[..., None]
    tools: dict[str, Tool] = field(init=False)

    def __post_init__(self) -> None:
        names = AGENT_TOOLS.get(self.agent, ())
        self.tools = {name: TOOLS[name] for name in names if name in TOOLS}

    def contract(self) -> str:
        """The tool section appended to the agent system prompt."""
        lines = [
            f"- {tool.name} {tool.signature} — {tool.description}"
            for tool in self.tools.values()
        ]
        return TOOL_CONTRACT_TEMPLATE.format(tools="\n".join(lines), turns=MAX_TOOL_TURNS)

    def _audit(self, **values: Any) -> None:
        try:
            run_store.save_tool_call(**values)
        except Exception:  # audit logging must never take the run down
            logger.exception("could not write tool_calls row tool=%s", values.get("tool"))

    def run(self, request: ToolRequest) -> dict[str, Any]:
        """Execute one request; never raises, always returns a result object."""
        name = request.tool.strip()
        started = time.perf_counter()
        started_at = run_store.now_iso()
        self.emit("tool_requested", step=self.step, agent=self.agent, tool=name)

        def finish(result: dict[str, Any], *, error: str | None = None) -> dict[str, Any]:
            duration_ms = int((time.perf_counter() - started) * 1000)
            self._audit(
                run_id=self.run_id,
                tool=name,
                started_at=started_at,
                duration_ms=duration_ms,
                status="failed" if error else "ok",
                error_message=error,
                args=json.dumps(request.args, ensure_ascii=False, default=str)[:MAX_ARGS_CHARS],
            )
            payload = _fit(result)
            self.emit(
                "tool_done",
                step=self.step,
                agent=self.agent,
                tool=name,
                ok=not error,
                ms=duration_ms,
                chars=len(json.dumps(payload.get("result", ""), ensure_ascii=False, default=str)),
            )
            return payload

        tool = self.tools.get(name)
        if tool is None:
            return finish(
                {"tool": name, "ok": False, "error": f"tool not allowed: {name}"},
                error=f"tool not allowed: {name}",
            )
        if self.deadline.expired():
            return finish(
                {"tool": name, "ok": False, "error": "run_out_of_time"},
                error="run_out_of_time",
            )
        missing = [key for key in tool.required if key not in request.args]
        if missing:
            return finish(
                {
                    "tool": name,
                    "ok": False,
                    "error": f"missing argument(s): {', '.join(missing)}",
                },
                error=f"missing argument(s): {', '.join(missing)}",
            )

        context = ToolContext(
            run_id=self.run_id,
            source=self.source,
            agent=self.agent,
            step=self.step,
            deadline=self.deadline,
        )
        try:
            result = tool.handler(request.args, context)
        except Exception as exc:  # noqa: BLE001 - a failing tool degrades, never ends the step
            message = redact(str(exc))[:300] or type(exc).__name__
            return finish({"tool": name, "ok": False, "error": message}, error=message)
        return finish({"tool": name, "ok": True, "result": result})
