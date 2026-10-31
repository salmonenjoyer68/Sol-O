<div align="center">
  <img src="./img/logo/sol-o.png" alt="Sol-O logo" width="180">

  <h1>Sol-O</h1>

  <p>
    <strong>Multi-agent orchestration for meaningful business impact.</strong>
  </p>
</div>

<p align="center">
  <a href="#about">About</a> •
  <a href="#phase-1-single-agent-api">Phase 1</a> •
  <a href="#quickstart">Quickstart</a> •
  <a href="#api">API</a> •
  <a href="#how-it-works">How it works</a> •
  <a href="#project-structure">Project structure</a> •
  <a href="#development">Development</a>
</p>

## About

Sol-O is a multi-agent AI orchestration platform designed to help businesses
turn complex goals into coordinated, measurable action.

Instead of relying on one general-purpose agent, Sol-O enables specialized
agents to collaborate. Each agent focuses on a specific responsibility, such
as research, analysis, planning, execution, quality assurance, or reporting.
An orchestration layer coordinates those agents, manages context, and combines
their work into a useful business outcome.

The goal is not simply to generate text. Sol-O is intended to help teams:

- automate repeatable business workflows;
- connect research, reasoning, and execution;
- reduce manual coordination between tasks and systems;
- produce traceable decisions and deliverables; and
- improve operational efficiency and business impact.

## Phase 1: single-agent API

Phase 1 is working today. It exposes **one** agent — the researcher — through a
FastAPI endpoint that always returns validated JSON.

What is implemented:

- `POST /run` accepts `{"agent": "researcher", "task": "..."}`.
- The system prompt is read from `agents/<agent>.md` on every request, so prompt
  edits apply without restarting the server.
- The LLM is called through any OpenAI-compatible endpoint configured in `.env`
  (Groq, OpenRouter, Gemini, Ollama, …).
- Responses are parsed as JSON and validated against a Pydantic contract.
- Invalid output is retried up to 2 times with the validation error fed back to
  the model.
- Markdown code fences are stripped before parsing.
- LLM calls time out after 30 seconds.
- Every call is logged with agent, task, attempts, duration, and result.

Not in Phase 1: orchestrator, multiple agents, search, database, auth,
streaming, and a frontend. The researcher prompt explicitly marks claims
unverified because no search tool exists yet.

## Quickstart

Requires Python 3.10+ and an OpenAI-compatible model endpoint.

### 1. Clone

```bash
git clone https://github.com/salmonenjoyer68/Sol-O.git
cd Sol-O
```

### 2. Create a virtual environment

```bash
python3 -m venv .venv
source .venv/bin/activate
```

On Windows PowerShell:

```powershell
python -m venv .venv
.venv\Scripts\Activate.ps1
```

### 3. Install dependencies

```bash
python -m pip install --upgrade pip
python -m pip install -r requirements.txt
```

`requirements.txt` contains the Phase 1 API dependencies. `uv sync` installs
the packages declared in `pyproject.toml` but not these, so use pip here.

### 4. Configure the model endpoint

```bash
cp .env.example .env
```

Edit `.env`:

| Variable | Meaning |
| --- | --- |
| `BASE_URL` | OpenAI-compatible API base URL, e.g. `https://api.groq.com/openai/v1` |
| `API_KEY` | Provider key, or any placeholder for a local endpoint |
| `MODEL` | Model identifier passed to the provider |

Examples:

```dotenv
# Groq
BASE_URL=https://api.groq.com/openai/v1
API_KEY=your-key
MODEL=llama-3.3-70b-versatile

# OpenRouter
BASE_URL=https://openrouter.ai/api/v1
API_KEY=your-key
MODEL=openai/gpt-4o-mini

# Local Ollama
BASE_URL=http://localhost:11434/v1
API_KEY=ollama
MODEL=llama3.2:1b
```

Keep `.env` private. It is gitignored and must never be committed.

### 5. Run the API

```bash
uvicorn app.main:app --reload
```

### 6. Send a task

In a second terminal:

```bash
python -m tests.test_run "What are the main risks of prompt injection?"
```

The client prints the validated JSON and exits non-zero if the response is not
valid. Point it elsewhere with `SOL_O_URL`, e.g.
`SOL_O_URL=http://localhost:8000/run`.

## API

### `POST /run`

Request:

```json
{
  "agent": "researcher",
  "task": "What are the main risks of prompt injection?"
}
```

Response (`200`):

```json
{
  "agent": "researcher",
  "status": "partial",
  "task": "Summarize the main risks of prompt injection.",
  "summary": "Short plain-language summary.",
  "findings": [
    {
      "claim": "One clear, single-assertion statement.",
      "source_url": "https://example.com/source",
      "confidence": "medium"
    }
  ],
  "conflicts": [],
  "gaps": ["What could not be verified."],
  "clarification_needed": null
}
```

`status` is one of `complete`, `partial`, or `needs_clarification`.
`confidence` is one of `high`, `medium`, or `low`.

Errors:

| Status | Body | Cause |
| --- | --- | --- |
| `404` | `{"error": "agent_not_found", ...}` | No `agents/<agent>.md` |
| `502` | `{"error": "invalid_agent_output", "raw": "..."}` | Output still invalid after retries |
| `502` | `{"error": "llm_error", ...}` | Provider or configuration failure |
| `504` | `{"error": "llm_timeout", ...}` | No response within 30 seconds |

## How it works

Phase 1 runs a single agent:

```text
POST /run
   │
   ▼
load agents/<agent>.md          (per request)
   │
   ▼
OpenAI-compatible LLM           (BASE_URL, API_KEY, MODEL)
   │
   ▼
strip fences → parse JSON → validate with Pydantic
   │                         │
   │ valid                   │ invalid (up to 2 retries)
   ▼                         ▼
200 + JSON              feed error back → retry → 502 if still invalid
```

The planned multi-agent workflow adds an orchestrator (the "Sun") that routes
work across specialist agents (the "planets"):

```text
Business goal
      │
      ▼
Orchestrator
      │
      ├── Research agent
      ├── Analysis agent
      ├── Planning agent
      ├── Execution agent
      └── Review agent
      │
      ▼
Validated result and business action
```

A business request would be decomposed as follows:

1. The **orchestrator** receives the business objective.
2. A **research agent** gathers the relevant information.
3. An **analysis agent** identifies insights, risks, and opportunities.
4. A **planning agent** turns the insights into an action plan.
5. An **execution agent** carries out approved actions through connected tools.
6. A **review agent** checks the result before it is delivered.

## Project structure

```text
.
├── agents/
│   └── researcher.md     # Agent system prompts, one file per agent
├── app/
│   ├── main.py           # FastAPI app and POST /run
│   ├── llm.py            # OpenAI-compatible LLM wrapper
│   └── schemas.py        # Pydantic request/response contracts
├── configs/              # Versioned workflow and model configuration
├── data/                 # Gitignored datasets (raw → interim → processed)
├── deployment/           # Container and release files
├── docs/                 # Design notes and project documentation
├── experiments/          # Reproducible experiments and outputs
├── img/                  # Logo and documentation assets
├── models/               # Model metadata and local checkpoints
├── notebooks/            # Exploration and analysis notebooks
├── scripts/              # Operational and automation scripts
│   └── cron/             # Standalone TypeScript cron utility (vitest)
├── src/sol_o/            # Installable package (placeholders for later phases)
│   ├── data/             # Ingestion and data validation
│   ├── features/         # Feature extraction and transformations
│   ├── models/           # Training, evaluation, and inference logic
│   ├── pipelines/        # End-to-end workflow orchestration
│   ├── serving/          # Prediction-serving adapters
│   └── utils/            # Shared utilities
├── tests/
│   ├── integration/      # Cross-component tests
│   ├── unit/             # Focused tests
│   └── test_run.py       # Terminal client for /run
├── .env.example          # Environment template (no secrets)
├── requirements.txt      # Phase 1 dependencies
└── pyproject.toml        # Packaging, pytest, and ruff configuration
```

## Development

Lint:

```bash
uvx ruff check .
```

Tests (install pytest first; it is not a project dependency yet):

```bash
python -m pip install pytest
python -m pytest
```

Cron utility tests (isolated TypeScript project):

```bash
cd scripts/cron
npm install
npm test
```

Contributions should follow the Phase 1 contracts in `app/schemas.py`: every
agent output is validated before it leaves the API.

## Development principles

- Keep agents specialized and their responsibilities explicit.
- Make workflows observable, reproducible, and easy to evaluate.
- Validate agent outputs before passing them to the next stage.
- Require human approval before irreversible or high-impact actions.
- Keep raw data, secrets, generated artifacts, and large model files out of Git.

## License

License information will be added when the project license is selected.
