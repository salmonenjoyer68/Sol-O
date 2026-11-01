# ADR 0001: Planets request tools in JSON; the Sun executes them

- Status: accepted
- Date: 2026-11-01

## Context

Sol-O's planets answered in one strict JSON object, and every tool was run by the
Sun before or after the LLM: `_search_context` before the researcher, `_send_telegram`
in the notifier step. Planets could not fetch a page, consult an internal knowledge
base, do arithmetic, or look at earlier runs of the same conversation.

The runtime target is a local OpenAI-compatible endpoint (Ollama). `app/llm.py` already
degrades `response_format` from `json_schema` to `json_object` to none, because many
endpoints reject strict schemas.

## Decision

Planets request tools inside their JSON output. A planet may return exactly
`{"tool_requests": [{"tool": "<name>", "args": {...}}]}`; the Sun validates the request
against a per-agent allowlist, runs the handler, records one `tool_calls` row, emits the
tool events, and appends the results as another user turn. At most 3 tool turns per step.
Provider-native function calling (`tools=` / `tool_choice`) is not used.

## Why

- Every OpenAI-compatible endpoint supports plain chat completions; native tool calling
  needs a capability matrix (tools + `json_schema` + strict) that local runtimes do not
  reliably satisfy.
- The strict output contract, `parse_output`, and the 3-attempt validation retry keep
  working unchanged: the envelope is detected before validation, so an unrecognised or
  disallowed request degrades into the existing retry path.
- Each tool call gets exactly one auditable row in `tool_calls` with its arguments, which
  is what "real users soon" needs; native tool calls would have to be reconstructed from
  provider-specific response objects.
- The loop lives in one place (`app/agents.py::call_validated`), so `/run`, the planner and
  the system steps keep their current behaviour by passing no toolset.

## Cost

One extra LLM round trip per tool turn, capped at 3, drawn from the same wall-clock
`RUN_TIMEOUT_SECONDS` budget as everything else. A weak local model may fail to emit the
envelope or the final object; that consumes a validation retry and, at worst, fails the
step as it does today.
