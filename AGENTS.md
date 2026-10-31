# AGENTS.md

Sol-O is an early-stage Python scaffold for a multi-agent orchestration platform. Most directories contain only `.gitkeep`; expect to create the real implementation.

## Setup

- Repo lives in WSL/Linux. The venv is Linux-layout: `source .venv/bin/activate` (ignore the README's Windows path).
- `.venv` was created by uv 0.13.1 and `uv.lock` exists. Prefer `uv sync`; `pip install -e .` (README) also works.
- Copy `.env.example` to `.env` for local secrets. `MODEL_PROVIDER`, `MODEL_NAME`, and `API_BASE_URL` are placeholders not yet read by code.

## Commands

- Tests: `python -m pytest` (configured `testpaths = ["tests"]`). No tests exist yet; put new ones in `tests/unit/` or `tests/integration/`.
- `pytest` is **not** declared in `pyproject.toml` or installed in `.venv`; install it before expecting tests to run.
- Lint: ruff is configured (`line-length = 100`, `target-version = "py310"`) but is not a dependency and not in `.venv`. Install it or run `uvx ruff check .`.
- No CI, pre-commit, Makefile, tox, or nox. Do not assume a pipeline exists.

## Layout and architecture

- `src/sol_o/` is the only installable package (`sol-o`), found via `[tool.setuptools.packages.find] where = ["src"]`. Import as `sol_o`.
- Subpackages `data/`, `features/`, `models/`, `pipelines/`, `serving/`, `utils/` are empty placeholders. Put orchestration logic in `pipelines/`, service entrypoints in `app/`, workflow/model config in `configs/`.
- `agent/*.md` are agent system prompts, one file per agent, not Python modules. The orchestrator is the "Sun"; agents are "planets". `agent/researcher.md` defines a strict JSON-only output contract (fixed keys and enum values) — preserve it when editing.
- Runtime dependencies are only `langchain` and `langchain-openai`.

## Conventions and gotchas

- `requires-python = ">=3.10"`, but the checked-in `.venv` is Python 3.12.
- `data/`, `models/`, `artifacts/`, and `experiments/` contents are gitignored (only `.gitkeep` is tracked). Do not commit datasets, checkpoints, or generated outputs.
- `.env` is gitignored; never commit secrets.
- Project principles that affect design: keep agents single-purpose, validate outputs between stages, and require human approval before irreversible/high-impact actions.
