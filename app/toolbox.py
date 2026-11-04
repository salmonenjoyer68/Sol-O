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
from app.config import settings
from app.reliability import Deadline, redact
from app.schemas import ToolRequest
from tools.calc import calculate
from tools.fetch import fetch_url
from tools.kb import search_kb
from tools.recall import recall

logger = logging.getLogger("sol_o.sun")

TOOL_CONTRACT_TEMPLATE = """
## Tool envelope (runtime contract)

You may answer with your normal output object, or ask the Sun to run tools first. To ask, return exactly this object and nothing else:
{{"tool_requests": [{{"tool": "<name>", "args": {{...}}}}]}}

The envelope has no `status` key; it must contain only the `tool_requests` key. The Sun checks each request against the allowlist below, executes it, and calls you again with "Tool results (JSON):" holding one object per request with `tool`, `ok`, `truncated` and either `result` or `error`. A result marked `truncated: true` was capped at some layer (the tool itself or the result budget): treat it as partial, never as complete. The Sun executes tool requests; you only ask. Never claim to have run a tool yourself.

Available tools (this rendered list is the only source of allowed names):
{tools}

Rules: at most {turns} tool turns per step. After that you must return your normal output object. Never request a tool that is not listed above. A requested `limit` outside 1..{items} is clamped into that range before the tool runs.
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
            "Search the company knowledge base; returns matching chunks with their file path "
            "(trimmed to the result budget; `limit` is clamped to the documented range)."
        ),
        required=("query",),
        handler=_handler_search,
    ),
    "fetch_url": Tool(
        name="fetch_url",
        signature='{"url": str}',
        description=(
            "Download one http(s) page and return its visible text (trimmed to the result "
            "budget and marked `truncated` when partial)."
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
        description=(
            "Search earlier runs from this same conversation (trimmed to the result budget)."
        ),
        required=("query",),
        handler=_handler_recall,
    ),
}

AGENT_TOOLS: dict[str, tuple[str, ...]] = {
    "researcher": ("kb_search", "fetch_url", "recall"),
    "writer": ("kb_search", "calc", "recall"),
    "checker": ("kb_search", "calc"),
}


ELLIPSIS = "…"


def _serialized(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, default=str)


def _fits(make: Callable[[Any], dict[str, Any]], value: Any) -> bool:
    return len(_serialized(make(value))) <= settings.max_result_chars


def _clip(text: str, fits: Callable[[str], bool]) -> str | None:
    """Longest prefix of ``text`` plus an ellipsis that fits, or None when none does."""
    if fits(text):
        return text
    best: str | None = None
    low, high = 0, len(text)
    while low <= high:
        middle = (low + high) // 2
        candidate = text[:middle] + ELLIPSIS
        if fits(candidate):
            best = candidate
            low = middle + 1
        else:
            high = middle - 1
    return best


def _longest_text_key(value: dict[str, Any]) -> str | None:
    keys = [key for key, item in value.items() if isinstance(item, str)]
    if not keys:
        return None
    return max(keys, key=lambda key: len(_serialized(value[key])))


def _shrink(value: Any, make: Callable[[Any], dict[str, Any]]) -> Any:
    """One type-aware reduction of ``value``; ``None`` when it cannot shrink further.

    Strings are clipped, a list drops trailing items before its last item is
    shrunk, and a dict clips its longest text field.
    """
    if isinstance(value, str):
        return _clip(value, lambda candidate: _fits(make, candidate))
    if isinstance(value, list):
        if len(value) > 1:
            return value[:-1]
        if value:
            item = _shrink(value[0], lambda candidate: make([candidate]))
            if item is not None:
                return [item]
        return None
    if isinstance(value, dict):
        key = _longest_text_key(value)
        if key is None:
            return None
        clipped = _clip(value[key], lambda candidate: _fits(make, {**value, key: candidate}))
        if clipped is None or clipped == value[key]:
            return None
        return {**value, key: clipped}
    return None


def _fit(payload: dict[str, Any]) -> dict[str, Any]:
    """Trim a result until the serialized payload fits; never use a placeholder.

    ``tool``, ``ok`` and ``truncated`` always survive. ``truncated`` is true when
    the handler capped its own content (a ``result`` dict may carry
    ``truncated``) or when this function trimmed anything. When even the fixed
    envelope exceeds the cap, the envelope is returned intact and over the cap
    rather than losing its audit fields.
    """
    result = payload.get("result")
    capped = bool(payload.get("truncated", False))
    if isinstance(result, dict) and result.get("truncated") is True:
        capped = True
    envelope: dict[str, Any] = {
        "tool": payload["tool"],
        "ok": payload["ok"],
        "truncated": capped,
    }
    if "result" not in payload:
        return {**payload, "truncated": capped}

    def make(value: Any) -> dict[str, Any]:
        return {**envelope, "result": value}

    current = result
    if _fits(make, current):
        return make(current)
    envelope["truncated"] = True
    while not _fits(make, current):
        shrunk = _shrink(current, make)
        if shrunk is None:
            break
        current = shrunk
    if not _fits(make, current):
        # Serialization clipping is the last resort for values no type rule shrank.
        serialized = _serialized(current)
        clipped = _clip(serialized, lambda candidate: _fits(make, candidate))
        current = clipped if clipped is not None else ""
    return make(current)


def _clamp_limit(args: dict[str, Any]) -> dict[str, Any]:
    """Clamp a requested result count into 1..``settings.max_result_items``."""
    if "limit" not in args:
        return args
    try:
        requested = int(args["limit"])
    except (TypeError, ValueError):
        return args
    clamped = max(1, min(requested, settings.max_result_items))
    return args if args["limit"] == clamped else {**args, "limit": clamped}


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
        return TOOL_CONTRACT_TEMPLATE.format(
            tools="\n".join(lines),
            turns=settings.max_tool_turns,
            items=settings.max_result_items,
        )

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
        args = _clamp_limit(request.args)
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
                args=json.dumps(args, ensure_ascii=False, default=str)[
                    : settings.max_args_chars
                ],
            )
            payload = _fit(result)
            self.emit(
                "tool_done",
                step=self.step,
                agent=self.agent,
                tool=name,
                ok=not error,
                ms=duration_ms,
                truncated=payload["truncated"],
                chars=len(_serialized(payload.get("result", ""))),
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
        missing = [key for key in tool.required if key not in args]
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
            result = tool.handler(args, context)
        except Exception as exc:  # noqa: BLE001 - a failing tool degrades, never ends the step
            message = redact(str(exc))[:300] or type(exc).__name__
            return finish({"tool": name, "ok": False, "error": message}, error=message)
        return finish({"tool": name, "ok": True, "result": result})
