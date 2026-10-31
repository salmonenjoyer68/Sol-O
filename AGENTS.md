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

- API: `uvicorn app.main:app --reload`
- Terminal client: `python -m tests.test_run "your task"` (posts to `http://127.0.0.1:8000/run`; override with `SOL_O_URL`). Requires the API to be running.
- Tests: `python -m pytest` after `pip install pytest`. `testpaths = ["tests"]`; put tests in `tests/unit/` or `tests/integration/`. With no real test functions, `pytest` exits 5 ("no tests ran").
- Lint: `uvx ruff check .` (config: `line-length = 100`, `target-version = "py310"`).
- Cron utility (standalone TypeScript): `cd scripts/cron && npm test` (vitest); `npx tsc --noEmit` typechecks.
- No CI, pre-commit, Makefile, tox, or nox. Do not assume a pipeline exists.

## Layout and architecture

- `src/sol_o/` is the only installable package (`sol-o`), found via `[tool.setuptools.packages.find] where = ["src"]`. Import as `sol_o`.
- Subpackages `src/sol_o/{data,features,models,pipelines,serving,utils}/` are empty placeholders. Put orchestration logic in `pipelines/`, service entrypoints in `app/`, workflow/model config in `configs/`.
- Code lives in `app/`: `main.py` (FastAPI, `POST /run` and `POST /orchestrate`), `agents.py` (per-agent prompt loading, `OUTPUT_CONTRACT`, retrying `run_agent`), `orchestrator.py` (the Sun, `orchestrate`), `llm.py` (OpenAI-compatible client), `schemas.py` (Pydantic contracts). `app/` must stay importable as `app`, so do not move it under `src/`.
- Root `main.py`, `sun.py`, and `test_run.py` are thin compatibility entry points (`uvicorn main:app`, `python sun.py "task" [--plan]`, `python test_run.py "task"`), not copies of the app code.
- `agents/*.md` are agent system prompts, one file per agent, not Python modules. The orchestrator is the "Sun"; agents are "planets". `sun.md` is the planner prompt and is not a runnable planet.
- `tests/test_run.py` is a manual HTTP client, not a pytest test. Real automated tests belong in `tests/unit/` or `tests/integration/`.
- `scripts/cron/` is a self-contained TypeScript project (own `package.json`, lockfile, and `node_modules`, which is gitignored). Keep Node tooling there; do not mix it into `src/sol_o/`, the root, or the Python `tests/` tree.

## Phase 1 contracts

- `app/schemas.py` is authoritative. All models use `extra="forbid"`; `status` is `complete | partial | needs_clarification`, `confidence` is `high | medium | low`.
- The researcher output contract is duplicated in **three** places: `app/schemas.py`, `OUTPUT_CONTRACT` in `app/agents.py`, and the output-format section of `agents/researcher.md`. Changing fields or enum values requires updating all three.
- `POST /run`: loads `agents/<agent>.md` per request (name must match `^[A-Za-z0-9_-]+$`, else 404), appends `OUTPUT_CONTRACT`, then calls the LLM. Markdown fences are stripped before JSON parsing.
- Validation failures are retried twice (3 attempts total) with the error fed back to the model; final failure returns `502 {"error": "invalid_agent_output", "raw": ...}`.
- Errors: 404 unknown agent; 502 `llm_error` for provider/config failures (including missing env vars); 504 `llm_timeout` after 30 s.
- `agents/researcher.md` ends by stating no search tool exists. Preserve that constraint until a search tool is actually wired in.

## Phase 2 contracts

- `app/orchestrator.py` is authoritative for the Sun. `DEFAULT_PLAN = ("researcher", "writer", "checker")`; `MAX_STEPS = 7`; `use_llm_planner=True` plans through `agents/sun.md` with `PLAN_CONTRACT`.
- Runnable planets are the intersection of `agents/*.md` and `AGENT_OUTPUT_MODELS` (`AgentOutput`, `WriterOutput`, `CheckerOutput`). Adding an agent means a prompt file, a schema, and an entry in that dict.
- `checker` may not run before `writer` (`REQUIRED_PRIOR_AGENTS`).
- Each step gets the original task plus all earlier validated outputs. A step that exhausts its 3 attempts stops the run; the result keeps prior outputs.
- `OrchestrationResult.status` is `complete | failed | needs_clarification` (distinct from `AgentOutput.status`, which is `complete | partial | needs_clarification`); `planner` is `hardcoded | llm`.
- `POST /orchestrate` returns `200` even when the run stopped early — check `status` and `stopped_at`, not the HTTP code.
- All progress events go through `emit()` in `app/orchestrator.py`; keep them there so Phase 6 can swap printing for SSE/WebSockets.

## Conventions and gotchas

- `requires-python = ">=3.10"` and ruff targets `py310`, but the checked-in venv is 3.12. Do not use 3.11+ syntax or stdlib (`tomllib`, `ExceptionGroup`, `StrEnum`, `typing.Self`); `X | None` is fine at 3.10.
- Match existing style: `from __future__ import annotations` first, double quotes, 4-space indent, concise docstrings, small pure helpers (`parse_agent_output`, `load_system_prompt`).
- Phase 1 is plain Python plus the `openai` SDK. Do not introduce LangChain or another agent framework despite the `pyproject.toml` deps. Add new Phase 1 runtime deps to `requirements.txt` (and `pyproject.toml` only if they belong to the installable package).
- `data/`, `models/`, `artifacts/`, and `experiments/` contents are gitignored (only `.gitkeep` is tracked). Do not commit datasets, checkpoints, or generated outputs.
- `.env` is gitignored; never commit secrets. `.env.example` is the tracked template.
- Project principles that affect design: keep agents single-purpose, validate outputs between stages, and require human approval before irreversible/high-impact actions.
