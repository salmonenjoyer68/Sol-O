# Glossary

Terms Sol-O's code, prompts and UI rely on. One line each; the code is authoritative.

- **Tool turn** — one round trip in which a planet returns a tool envelope instead of its output, the Sun runs the requested tools, and the planet is called again with the results. Capped at `MAX_TOOL_TURNS` (3) per step (`src/config.rs`).
- **Tool envelope** — the exact JSON a planet returns to ask for tools: `{"tool_requests": [{"tool": <name>, "args": {...}}]}`. It carries no `status` key, which is how the Sun tells it apart from a normal agent output.
- **Allowlist** — `agent_tool_names` in `src/toolbox.rs`: which planets may request which tools. An unlisted tool is refused with `ok: false` and audited as a failed call.
- **Conversation source** — the `source` string on a run (`POST /inbound` or `POST /orchestrate`). It is the memory scope: `recall` only searches runs with the same source, so it must identify one conversation, not a channel.
- **Knowledge chunk** — a ≤800-character slice of one `kb/` file, carrying its file path and last `#` heading. `kb_search` ranks chunks by query-term overlap; there are no embeddings.
- **Toolset** — the per-step `Toolset` in `src/toolbox.rs` holding one planet's allowed tools plus the audit and event sinks; it renders the tool section of the system prompt and executes requests without ever raising.
- **Agent** — the unit the planner names: one `agents/<name>.md` prompt plus exactly one output model (`step_schema`/`validate_step_output` in `src/schemas.rs`). `sun` is an agent but not a runnable one.
- **Step** — one position in a run's plan: an agent call, or a code-only system step. "Planet" is the UI and prose word for a step; code identifiers use `agent` (`StepResult`, `PlannedStep`), while comments and test prose say planet freely.
- **System step** — a step the Sun always appends after the LLM plan (`SYSTEM_STEPS`: `logger`, then `notifier`). The planner never sees or chooses it.
- **Planet row** — one row of the planet roster, one per step: step number, agent, role, status, and progress. System and revision steps get their own rows, so a revised plan can show the same agent twice under different steps.
- **Stage** — the canvas surface between the rails, where the Sun sits at the centre with a planet per step on the orbit rings and the alignment marks completion; its text equivalent is the planet roster, which lists the same steps.
- **Run state** — the lifecycle label shown by the status pill and the Sun: idle, planning, running, waiting for approval, complete, partial, needs clarification, rejected, failed. An approval decision (approved or rejected) is a separate detail, not a run state.
- **Service** — the `sol-o` binary: `serve` exposes the HTTP API, SSE stream and UI; the other subcommands are the operator CLI. One process, one SQLite file.
- **Reference implementation** — the frozen Python tree (`app/`, `tools/`, their tests) kept after ADR 0002. It reads as documentation; no new features land there.
- **Surface texture** — the detail inside a planet's disc; excludes the halo, the ring band and the label.
- **Ring band** — the ring drawn around one planet; distinct from the orbit ring the planet travels on.
- **Orbit ring** — the dashed path a planet travels along; distinct from the ring band around the planet itself.
- **Photosphere** — the Sun's disc; the Sun's counterpart of a planet's surface texture.
