# AGENTS.md

Sol-O is a multi-agent orchestration platform. The backend is Rust: one binary, `sol-o`,
built from `src/`. The browser UI is the single file `web/index.html`. Write real
implementation code, not scaffolds.

`app/`, `tools/` and `tests/` are the **frozen Python reference implementation** (see
[Frozen reference](#frozen-reference)). They are not built, not tested, and not imported by
anything the Rust backend runs.

## Setup

- Rust stable, edition 2021. No virtualenv, no Python and no Node are needed to build, test
  or run the backend. Run commands from the repo root: `ROOT` is baked in at build time and
  `agents/`, `kb/` and `web/` resolve under it.
- `cp .env.example .env` and fill `BASE_URL`, `API_KEY`, `MODEL`. Tools want
  `TAVILY_API_KEY` and `TELEGRAM_BOT_TOKEN`/`TELEGRAM_CHAT_ID`; `DRY_RUN` defaults to true.
- The binary loads `.env` once at startup (`dotenvy`, from the repo root). `SOL_O_ENV_FILE`
  selects another file; an empty value loads nothing. Restart after editing `.env`; prompt
  edits (`agents/*.md`) need no restart — they are read per request.
- `SOL_O_DB` overrides the SQLite file (default `data/sol-o.db`, created on first run).

## Commands

- Serve: `cargo run -- serve` → `http://127.0.0.1:8000/` (UI), `GET /stream` (SSE).
  `--host` and `--port` override the defaults; the Rust service owns port 8000.
- Run a task: `cargo run -- run "your task" [--plan] [--file examples/inquiry.txt]
  [--source <conversation>]`. With no task it reads `TASK_DESCRIPTION` from the environment.
- Inspect: `cargo run -- show <run_id>`, `cargo run -- pending`,
  `cargo run -- approve <id>`, `cargo run -- reject <id>`.
- Tests: `cargo test`. Unit tests live in `#[cfg(test)]` modules beside the code;
  `rust-tests/*.rs` are black-box suites that spawn the real binary against
  `rust-tests/support/fake_provider.rs` (a scripted OpenAI-compatible provider). Fixtures are
  in `rust-tests/fixtures/`.
- Format: `cargo fmt` (optional lint: `cargo clippy --all-targets`). The tree is not
  rustfmt-clean yet, so format only the files you touch. There is no CI, pre-commit, Makefile
  or tox; do not assume a pipeline exists.

## Layout

- `src/main.rs` starts `tracing` on stderr and calls `src/cli.rs`; `src/lib.rs` lists the
  modules. `src/http.rs` is the axum router, `src/cli.rs` the `clap` subcommands,
  `src/orchestrator.rs` the Sun, `src/agents.rs` prompt loading/validation/retries,
  `src/toolbox.rs` the tool registry, allowlists and result budget, `src/persistence.rs` the
  SQLite schema and queries, `src/events.rs` the bus and SSE stream, `src/approvals.rs` the
  approval gate, `src/config.rs` settings, `src/llm.rs` the provider client,
  `src/reliability.rs` retries/deadline/redaction, `src/schemas.rs` the wire contracts. Tool
  handlers: `src/kb.rs` (`kb_search`), `src/fetch.rs` (`fetch_url`), `src/calc.rs` (`calc`),
  `src/recall.rs` (`recall`), plus Sun-side `src/search.rs` (Tavily) and `src/telegram.rs`.
- `agents/*.md` are agent system prompts, one file per agent. The orchestrator is the Sun;
  agents are planets. `sun.md` is the planner prompt and is not a runnable planet.
- `kb/` is the committed knowledge base read by `kb_search`; `rust-tests/` are the
  integration suites; `examples/` holds sample tasks.
- `scripts/cron/` is a standalone TypeScript utility with its own `package.json`. Keep Node
  tooling there; the backend never touches it.

## Contracts a change must keep

- **Routes** (11, all in `src/http.rs`): `GET /`, `POST /run`, `POST /orchestrate`,
  `POST /inbound`, `GET /runs`, `GET /runs/{run_id}`, `GET /runs/{run_id}/events`,
  `GET /stream`, `GET /pending`, `POST /pending/{id}/approve`, `POST /pending/{id}/reject`.
  Status codes and the exact `error` codes (`agent_not_found`, `agent_unreadable`,
  `llm_timeout`, `llm_error`, `invalid_agent_output`, `run_not_found`, `not_found`,
  `conflict`, `execution_failed`, `ui_unreadable`, `invalid_request`, `database_error`) are
  part of the contract. CORS admits localhost origins only.
- **Events**: `{run_id, seq, ts, event, data}` with a per-run increasing `seq`, stored in
  `run_events` and streamed over SSE with `Last-Event-ID` backlog replay and a 15 s
  keepalive. Names and fields are fixed: `run_started`, `plan_ready`, `agent_called`,
  `agent_working`, `tool_requested`, `tool_done`, `agent_done`, `agent_failed`,
  `revision_requested`, `search_done`, `search_failed`, `logger_saved`,
  `logger_failed`, `approval_required`, `awaiting_approval`, `notifier_pending`,
  `notifier_sent`, `notifier_skipped`, `notifier_failed`, `approval_decided`, `all_done`,
  `run_failed`, `stopped`. The UI reads only these; renaming one breaks it.
- **Database**: five tables — `runs`, `agent_calls`, `tool_calls`, `pending_actions`,
  `run_events` — with the exact columns in `src/persistence.rs`. `ensure_columns` upgrades an
  older `sol-o.db` in place, so an existing file must keep working. Timestamps are ISO-8601
  UTC with second precision; run ids are 32 lowercase hex characters.
- **Prompts**: the runtime output contracts are plain strings in `src/agents.rs`
  (`OUTPUT_CONTRACT`, `STEP_CONTRACT`, `PLAN_CONTRACT`) appended to the prompt file, and
  `agents/*.md` documents the same shape as a JSON example. `src/schemas.rs` is
  authoritative; changing an output field means updating the schema, the contract strings
  and the prompt file together, or the model is told two different things.
- **Tool envelope**: `{"tool_requests": [{"tool": "<name>", "args": {...}}]}` with no
  `status` key. `agent_tool_names` in `src/toolbox.rs` is the only source of allowed names
  (researcher: `kb_search`, `fetch_url`, `recall`; writer: `kb_search`, `calc`, `recall`;
  checker: `kb_search`, `calc`). Each result keeps `tool`, `ok`, `truncated`; the
  result budget (`MAX_RESULT_CHARS`, `MAX_RESULT_ITEMS`) and `MAX_ARGS_CHARS` come from
  `src/config.rs` and are never hardcoded elsewhere.
- **Approval gate**: with `REQUIRE_APPROVAL=true` and `AUTO_APPROVE=false`, a run that is
  ready to notify stops as `awaiting_approval` with one `pending_actions` row and sends
  nothing. Approve executes the stored message exactly once and restores the stored
  `run_status`; reject sets `rejected`; actions expire after `PENDING_TTL_HOURS` and cannot
  be decided twice.
- **Two allowed deviations from the reference**: the validation-error text fed back to the
  model is Rust-shaped, and log lines are `tracing` records instead of `INFO:sol_o.sun:…`.
  Everything else — payload shapes, event names and fields, truncation limits, `calc`
  arithmetic, ISO formats — is parity-locked.
- `web/index.html` stays a single self-contained file (vanilla JS + Canvas): no npm, bundler
  or CDN. Its only external request is one Google Fonts stylesheet with a system fallback.

## Conventions

- Keep the reference's semantics when editing `src/`: codepoint-based truncation, Python's
  `//`, `%`, int/float promotion and `round(v, 6)` in `calc`, `json.dumps(default=str)` for
  fallback serialization.
- Settings live in `src/config.rs` and are read once at process start. Tests are hermetic:
  point `SOL_O_ENV_FILE` at an empty file and `SOL_O_DB` at a temp path so a developer `.env`
  never reaches a test.
- `.env` is gitignored; never commit secrets. `.env.example` is the tracked template. `data/`
  contents are gitignored.

## Frozen reference

`app/` (FastAPI backend), `tools/` (tool handlers) and `tests/` (pytest suite plus the manual
`tests/test_run.py` HTTP client) are the Python implementation the Rust backend was ported
from, kept so behaviour can be read and compared. Treat them as read-only: no features, no
contract changes, no new dependencies. They are a reading aid, not a fallback — never wire
them into the build, the test run or the runtime, and never put a virtualenv on the backend
path. The checked-in `.venv` and `requirements.txt` still start the reference if you want a
side-by-side comparison; that is the only place they belong. `docs/adr/` and
`GLOSSARY.md` are the live decision and terminology docs.

## Agent skills

- **Issue tracker**: GitHub Issues on `salmonenjoyer68/Sol-O` via the `gh` CLI. See
  `docs/agents/issue-tracker.md`.
- **Triage labels**: five canonical roles, default strings `needs-triage`, `needs-info`,
  `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.
- **Domain docs**: single context — root `GLOSSARY.md` plus `docs/adr/`. See
  `docs/agents/domain.md`.
