# AGENTS.md

Sol-O is an early-stage Python project for a multi-agent orchestration platform. Phase 1 (single agent behind FastAPI) and Phase 2 (the Sun orchestrator) are implemented in `app/`; `src/sol_o/` and most other directories are still placeholders. Expect to write real implementation code, not just tweak scaffolds.

## Setup

- Repo lives in WSL/Linux. Activate the checked-in venv with `source .venv/bin/activate`; it is Python 3.12, laid out for Linux. The README's Windows activation path does not apply.
- Dependencies are split:
  - `uv sync` installs `pyproject.toml` deps (`langchain`, `langchain-openai`) — **Phase 1 does not use them**.
  - `pip install -r requirements.txt` installs the Phase 1 runtime: `fastapi`, `uvicorn`, `openai`, `pydantic`, `python-dotenv`.
- `pytest` and `ruff` are **not** installed in `.venv` (and `pytest` is not declared in `pyproject.toml`). Install them or use `uvx`.
- `cp .env.example .env`; `app/llm.py` reads `BASE_URL`, `API_KEY`, `MODEL` from the repo-root `.env` **once at import time**. Restart the server after editing `.env`; prompt edits need no restart.
- Run Python commands from the repo root. `app` is a top-level, non-installed package; the only installable package is `sol_o`. The TypeScript utility is the exception: run it from `scripts/cron/`.

## Commands

- API: `uvicorn app.main:app --reload` (open `http://127.0.0.1:8000/` for the live solar-system UI; `GET /stream` is the SSE feed)
- Terminal client: `python -m tests.test_run "your task"` (posts to `http://127.0.0.1:8000/run`; override with `SOL_O_URL`). Requires the API to be running.
- Tests: `python -m pytest` after `pip install pytest`. `testpaths = ["tests"]`; put tests in `tests/unit/` or `tests/integration/`. With no real test functions, `pytest` exits 5 ("no tests ran").
- Lint: `uvx ruff check .` (config: `line-length = 100`, `target-version = "py310"`).
- Cron utility (standalone TypeScript): `cd scripts/cron && npm test` (vitest); `npx tsc --noEmit` typechecks.
- No CI, pre-commit, Makefile, tox, or nox. Do not assume a pipeline exists.

## Layout and architecture

- `src/sol_o/` is the only installable package (`sol-o`), found via `[tool.setuptools.packages.find] where = ["src"]`. Import as `sol_o`.
- Subpackages `src/sol_o/{data,features,models,pipelines,serving,utils}/` are empty placeholders. Put orchestration logic in `pipelines/`, service entrypoints in `app/`, workflow/model config in `configs/`.
- Code lives in `app/`: `main.py` (FastAPI: `/run`, `/orchestrate`, `/runs`, `/runs/{id}`, `/inbound`, `/pending`), `agents.py` (prompt loading, validation, retries), `orchestrator.py` (the Sun), `logger.py` (SQLite runs/calls/pending), `approvals.py` (approval gate), `config.py` (env settings), `reliability.py` (retries, deadline, redaction), `llm.py`, `schemas.py`. `app/` must stay importable as `app`, so do not move it under `src/`.
- Root `main.py`, `sun.py`, and `test_run.py` are thin compatibility entry points (`uvicorn main:app`, `python sun.py "task" [--plan] [--file path]`, `python test_run.py "task"`), not copies of the app code. `sun.py` also handles `--show <run_id>`, `--pending`, `--approve <id>`, `--reject <id>`; with no task it reads `TASK_DESCRIPTION` from `.env`.
- `agents/*.md` are agent system prompts, one file per agent, not Python modules. The orchestrator is the "Sun"; agents are "planets". `sun.md` is the planner prompt and is not a runnable planet.
- `tests/test_run.py` is a manual HTTP client, not a pytest test. Real automated tests belong in `tests/unit/` or `tests/integration/`.
- `scripts/cron/` is a self-contained TypeScript project (own `package.json`, lockfile, and `node_modules`, which is gitignored). Keep Node tooling there; do not mix it into `src/sol_o/`, the root, or the Python `tests/` tree.
- `web/index.html` is the Phase 6 solar-system UI, served by FastAPI at `/`. Keep it a single self-contained file: no npm, bundler, or CDN libraries.

## Phase 1 contracts

- `app/schemas.py` is authoritative. All models use `extra="forbid"`; `status` is `complete | partial | needs_clarification`, `confidence` is `high | medium | low`.
- The researcher output contract is duplicated in **three** places: `app/schemas.py`, `OUTPUT_CONTRACT` in `app/agents.py`, and the output-format section of `agents/researcher.md`. Changing fields or enum values requires updating all three.
- `POST /run`: loads `agents/<agent>.md` per request (name must match `^[A-Za-z0-9_-]+$`, else 404), appends `OUTPUT_CONTRACT`, then calls the LLM. Markdown fences are stripped before JSON parsing.
- Validation failures are retried twice (3 attempts total) with the error fed back to the model; final failure returns `502 {"error": "invalid_agent_output", "raw": ...}`.
- Errors: 404 unknown agent; 502 `llm_error` for provider/config failures (including missing env vars); 504 `llm_timeout` after 30 s.
- `agents/researcher.md` has no search tool of its own: the Sun runs Tavily and injects the results. When results are empty, the researcher must mark claims `unverified`.

## Phase 2/3 contracts

- `app/orchestrator.py` is authoritative for the Sun. `DEFAULT_PLAN = ("researcher", "writer", "checker")`; `MAX_STEPS = 7` LLM steps; `use_llm_planner=True` plans through `agents/sun.md` with `PLAN_CONTRACT`.
- Runnable LLM planets are the intersection of `agents/*.md` and `AGENT_OUTPUT_MODELS` (`AgentOutput`, `WriterOutput`, `CheckerOutput`). Adding one means a prompt file, a schema, and an entry in that dict.
- `checker` may not run before `writer` (`REQUIRED_PRIOR_AGENTS`).
- Each step gets the original task plus all earlier validated outputs. A step that exhausts its 3 attempts stops the run; the result keeps prior outputs.
- Checker loop: `verdict == "needs_changes"` appends one writer revision and one checker rerun. A second `needs_changes` ends the run with `status="partial"` and the issues preserved. Never loop twice.
- `logger` and `notifier` are code-only system steps in `SYSTEM_STEPS`, always appended after the LLM plan, in that order, on every run (success, failure, or clarification). The planner never sees them.
- `app/logger.py` stores one row per run in `data/sol-o.db` (`SOL_O_DB` overrides). It inserts a `running` row at start so `/inbound` run ids resolve immediately, then updates it at the end. Logging never crashes a run.
- `OrchestrationResult.status` is `complete | partial | failed | needs_clarification | awaiting_approval | rejected` (distinct from `AgentOutput.status`); `planner` is `hardcoded | llm`; `run_id` is the DB row id. Failed runs also carry `failed_at` and `failure` (`FailureType`: `invalid_json`, `schema_mismatch`, `timeout`, `llm_error`, `tool_error`) with `raw_output` truncated to 2000 chars.
- `POST /orchestrate` queues the run via `BackgroundTasks` and returns `{"run_id", "status": "accepted"}` immediately so the browser can stream it live; read the outcome from `GET /runs/{run_id}`. `POST /inbound` behaves the same with `202 {"run_id", "status": "accepted"}`. Both return `2xx` even when the run stops early — check the stored `status`, `stopped_at`. `GET /runs?limit=20` returns newest rows with `outputs` parsed from `outputs_json`.
- All progress events go through `emit()` in `app/orchestrator.py`; it logs with timestamps and forwards to the Phase 6 event bus (`app/events.py`) bound to the run id.

## Phase 4/5 contracts

- `app/config.py` loads `.env` once into `settings`: `LLM_TIMEOUT_SECONDS` (30), `LLM_MAX_RETRIES` (2), `TOOL_TIMEOUT_SECONDS` (15), `RUN_TIMEOUT_SECONDS` (180), `MAX_AGENTS` (7), `REQUIRE_APPROVAL` (true), `AUTO_APPROVE` (false), `PENDING_TTL_HOURS` (24), `MAX_INPUT_CHARS` (8000). Never hardcode these values elsewhere.
- `app/reliability.py` owns `call_with_retries` (1s/2s/4s backoff), `Deadline`, `redact`, and the root-handler `RedactFilter`. LLM retries only timeout/connection/429/5xx; 4xx fails immediately. Tools retry once on timeout/5xx. Redaction covers env secrets, `sk-...`, `bot<id>:...` and bearer tokens, including httpx request URLs.
- The whole run uses `RUN_TIMEOUT_SECONDS`; exhaustion yields `status="failed"`, `reason="run_timeout"`, `error_type="timeout"`. Per-call timeout is `min(LLM_TIMEOUT_SECONDS, remaining)`.
- Tasks over `MAX_INPUT_CHARS` are truncated and the run row/result carry `warning`.
- `agent_calls` (one row per LLM call, including the planner at step 0) and `tool_calls` (search, Telegram) are written even on failure. `GET /runs/{id}` returns run + calls + pending actions; `GET /runs?status=` filters.
- Approval gate: irreversible notifications are gated when `REQUIRE_APPROVAL=true` and `AUTO_APPROVE=false`. Successful/partial/clarification runs stop as `awaiting_approval` with a `pending_actions` row and no send; failed runs skip the notifier and stay `failed`. Approve executes the stored message exactly once (dry-run still prints) and restores the stored `run_status`; reject sets `rejected`. Actions expire after `PENDING_TTL_HOURS` and cannot be decided twice.
- `tools/` holds code-run tools: `search.py` (Tavily over httpx, `TAVILY_API_KEY`) and `telegram.py` (Bot API, `TELEGRAM_BOT_TOKEN`/`TELEGRAM_CHAT_ID`, `DRY_RUN` default true). The model never invokes them; the Sun does.

## Phase 6 contracts

- `app/events.py` is an in-memory bus: events are `{run_id, seq, ts, event, data}` with a per-run increasing `seq` (plus a process-wide `id` used as the SSE event id). `emit()` binds to the run via the `run_context` ContextVar; publishing redacts secrets and writes every event to the `run_events` table in `app/logger.py`.
- Real events: `run_started{task}`, `plan_ready{agents, planner, steps}`, `agent_called{step, agent}`, `agent_working{step, agent, attempt}` (each LLM attempt, plus search and Telegram), `agent_done{step, agent, status, summary (<=140 chars)}`, `agent_failed{step, agent, error_type, message}`, `awaiting_approval{pending_id, preview}`, `approval_decided{pending_id, decision, run_status}`, `all_done{status, seconds}` (complete/partial/clarification) and `run_failed{failed_at, error_type, message}`. `awaiting_approval` runs emit no terminal event until the approval is decided.
- API: `GET /stream` is SSE (`text/event-stream`) for all runs, keepalive every 15 s, backlog replay from the `Last-Event-ID` header, one queue per tab. `GET /runs/{run_id}/events` returns saved events for replay. `GET /` serves the single-file UI. CORS is localhost/127.0.0.1 only.
- UI: `web/index.html` is one self-contained file (vanilla JS + Canvas, no build/CDN). It is driven only by real events; local motion is cosmetic (orbits, twinkle, glow, alignment easing). Clicking a planet reads its output from `GET /runs/{run_id}`; replay fetches saved events and caps every inter-event gap at 1.5 s. A result window auto-opens on `all_done`, `run_failed` or `approval_decided`, showing the final draft, clarification, failure or rejection, and can be reopened by clicking the status pill.

## Conventions and gotchas

- `requires-python = ">=3.10"` and ruff targets `py310`, but the checked-in venv is 3.12. Do not use 3.11+ syntax or stdlib (`tomllib`, `ExceptionGroup`, `StrEnum`, `typing.Self`); `X | None` is fine at 3.10.
- Match existing style: `from __future__ import annotations` first, double quotes, 4-space indent, concise docstrings, small pure helpers (`parse_agent_output`, `load_system_prompt`).
- Phase 1 is plain Python plus the `openai` SDK and `httpx` for tools. Do not introduce LangChain or another agent framework despite the `pyproject.toml` deps. Runtime deps go in `requirements.txt`; `httpx` is also in `pyproject.toml` so `uv run python sun.py` works.
- `data/`, `models/`, `artifacts/`, and `experiments/` contents are gitignored (only `.gitkeep` is tracked). Do not commit datasets, checkpoints, or generated outputs.
- `.env` is gitignored; never commit secrets. `.env.example` is the tracked template.
- Project principles that affect design: keep agents single-purpose, validate outputs between stages, and require human approval before irreversible/high-impact actions.
