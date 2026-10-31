# AGENTS.md

Sol-O is an early-stage Python scaffold for a multi-agent orchestration platform. Most directories contain only `.gitkeep`; expect to create the real implementation.

## Setup

- Repo lives in WSL/Linux. The venv is Linux-layout: `source .venv/bin/activate` (ignore the README's Windows path).
- `.venv` was created by uv 0.13.1 and `uv.lock` exists. Prefer `uv sync`; `uv sync` does not install the Phase 1 API deps, so use `pip install -r requirements.txt` for those.
- Copy `.env.example` to `.env` for local secrets. `BASE_URL`, `API_KEY`, and `MODEL` are read by `llm.py` (OpenAI-compatible endpoint).

## Commands

- Tests: `python -m pytest` (configured `testpaths = ["tests"]`). No tests exist yet; put new ones in `tests/unit/` or `tests/integration/`.
- `pytest` is **not** declared in `pyproject.toml` or installed in `.venv`; install it before expecting tests to run.
- Lint: ruff is configured (`line-length = 100`, `target-version = "py310"`) but is not a dependency and not in `.venv`. Install it or run `uvx ruff check .`.
- No CI, pre-commit, Makefile, tox, or nox. Do not assume a pipeline exists.

## Layout and architecture

- `src/sol_o/` is the only installable package (`sol-o`), found via `[tool.setuptools.packages.find] where = ["src"]`. Import as `sol_o`.
- Subpackages `data/`, `features/`, `models/`, `pipelines/`, `serving/`, `utils/` are empty placeholders. Put orchestration logic in `pipelines/`, service entrypoints in `app/`, workflow/model config in `configs/`.
- `agents/*.md` are agent system prompts, one file per agent, not Python modules. The orchestrator is the "Sun"; agents are "planets". `agents/researcher.md` defines a strict JSON-only output contract (fixed keys and enum values) — preserve it when editing.
- Phase 1 lives in `app/`: `main.py` (FastAPI), `llm.py` (OpenAI-compatible client), `schemas.py` (Pydantic contracts); the terminal client is `tests/test_run.py` (`python -m tests.test_run "task"`). Dependencies are in `requirements.txt`; `pyproject.toml` still declares only `langchain` and `langchain-openai`.

## Conventions and gotchas

- `requires-python = ">=3.10"`, but the checked-in `.venv` is Python 3.12.
- `data/`, `models/`, `artifacts/`, and `experiments/` contents are gitignored (only `.gitkeep` is tracked). Do not commit datasets, checkpoints, or generated outputs.
- `.env` is gitignored; never commit secrets.
- Project principles that affect design: keep agents single-purpose, validate outputs between stages, and require human approval before irreversible/high-impact actions.
