# Glossary

Terms Sol-O's code and prompts rely on. One line each; the code is authoritative.

- **Tool turn** — one round trip in which a planet returns a tool envelope instead of its output, the Sun runs the requested tools, and the planet is called again with the results. Capped at `MAX_TOOL_TURNS` (3) per step (`app/config.py::settings.max_tool_turns`).
- **Tool envelope** — the exact JSON a planet returns to ask for tools: `{"tool_requests": [{"tool": <name>, "args": {...}}]}`. It carries no `status` key, which is how the Sun tells it apart from a normal agent output.
- **Allowlist** — `AGENT_TOOLS` in `app/toolbox.py`: which planets may request which tools. An unlisted tool is refused with `ok: false` and audited as a failed call.
- **Conversation source** — the `source` string on a run (`POST /inbound` or `POST /orchestrate`). It is the memory scope: `recall` only searches runs with the same source, so it must identify one conversation, not a channel.
- **Knowledge chunk** — a ≤800-character slice of one `kb/` file, carrying its file path and last `#` heading. `kb_search` ranks chunks by query-term overlap; there are no embeddings.
- **Toolset** — the per-step object (`app/toolbox.py`) holding one planet's allowed tools plus the audit and event sinks; it renders the tool section of the system prompt and executes requests without ever raising.
- **Agent** — the unit the planner names: one `agents/<name>.md` prompt plus exactly one output model (`AGENT_OUTPUT_MODELS`). `sun` is an agent but not a runnable one.
- **Step** — one position in a run's plan: an agent call, or a code-only system step. "Planet" is the UI and prose word for a step; it is not a code identifier.
- **System step** — a step the Sun always appends after the LLM plan (`SYSTEM_STEPS`: `logger`, then `notifier`). The planner never sees or chooses it.
- **Service** — the `sol-o` binary: `serve` exposes the HTTP API, SSE stream and UI; the other subcommands are the operator CLI. One process, one SQLite file.
- **Reference implementation** — the frozen Python tree (`app/`, `tools/`, their tests) kept after ADR 0002. It reads as documentation; no new features land there.
