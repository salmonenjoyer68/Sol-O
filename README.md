<div align="center">
  <img src="./img/logo/sol-o.png" alt="Sol-O logo" width="180">

  <h1>Sol-O</h1>

  <p>
    <strong>Multi-agent orchestration for meaningful business impact.</strong>
  </p>
</div>

<p align="center">
  <a href="#about">About</a> •
  <a href="#how-it-works">How it works</a> •
  <a href="#getting-started">Getting started</a> •
  <a href="#project-structure">Project structure</a>
</p>

## About

Sol-O is a multi-agent AI orchestration platform designed to help businesses
turn complex goals into coordinated, measurable action.

Instead of relying on one general-purpose agent, Sol-O enables specialized
agents to collaborate. Each agent can focus on a specific responsibility, such
as research, analysis, planning, execution, quality assurance, or reporting.
An orchestration layer coordinates those agents, manages context, and combines
their work into a useful business outcome.

The goal is not simply to generate text. Sol-O is intended to help teams:

- automate repeatable business workflows;
- connect research, reasoning, and execution;
- reduce manual coordination between tasks and systems;
- produce traceable decisions and deliverables; and
- improve operational efficiency and business impact.

## How it works

A typical Sol-O workflow can be organized like this:

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

For example, a business request could be decomposed into the following flow:

1. The **orchestrator** receives the business objective.
2. A **research agent** gathers the relevant information.
3. An **analysis agent** identifies insights, risks, and opportunities.
4. A **planning agent** turns the insights into an action plan.
5. An **execution agent** carries out approved actions through connected tools.
6. A **review agent** checks the result before it is delivered.

Agents should have clear responsibilities, explicit inputs and outputs, and
appropriate human approval for high-impact actions.

## Getting started

### 1. Clone the repository

```bash
git clone https://github.com/salmonenjoyer68/Sol-O.git
cd Sol-O
```

### 2. Create a virtual environment

```bash
python3 -m venv .venv
source .venv/bin/activate
```

On Windows PowerShell, use:

```powershell
python -m venv .venv
.venv\Scripts\Activate.ps1
```

### 3. Install the project

```bash
python -m pip install --upgrade pip
python -m pip install -e .
```

### 4. Configure your environment

Copy the example environment file and add the model provider settings you want
to use:

```bash
cp .env.example .env
```

Keep `.env` private. Do not commit API keys, credentials, or other secrets.

### 5. Start building an orchestration workflow

The repository is currently a foundation for the Sol-O agent system. Add
production orchestration code under `src/sol_o/`, application entrypoints
under `app/`, and workflow configuration under `configs/`.

Use the project checks while developing:

```bash
python -m pytest
```

When an application entrypoint is added, it can be started from the `app/`
directory or through the project command documented here.

## Project structure

```text
.
├── app/                  # Application entrypoints and API/UI integration
├── configs/              # Versioned workflow and model configuration
├── data/
│   ├── external/         # Data from third-party sources
│   ├── interim/          # Intermediate transformed data
│   ├── processed/        # Model-ready datasets
│   └── raw/              # Immutable source data
├── deployment/           # Container, infrastructure, and release files
├── docs/                 # Design notes and project documentation
├── experiments/          # Reproducible experiments and outputs
├── models/               # Model metadata and local checkpoints
├── notebooks/            # Exploration and analysis notebooks
├── scripts/              # Operational and automation scripts
├── src/sol_o/
│   ├── data/             # Ingestion and data validation
│   ├── features/         # Feature extraction and transformations
│   ├── models/           # Training, evaluation, and inference logic
│   ├── pipelines/        # End-to-end workflow orchestration
│   ├── serving/          # Prediction-serving adapters
│   └── utils/             # Shared utilities
├── tests/
│   ├── integration/      # Cross-component tests
│   └── unit/              # Focused tests
└── pyproject.toml         # Python project and tooling configuration
```

## Development principles

- Keep agents specialized and their responsibilities explicit.
- Make workflows observable, reproducible, and easy to evaluate.
- Validate agent outputs before passing them to the next stage.
- Require human approval before irreversible or high-impact actions.
- Keep raw data, secrets, generated artifacts, and large model files out of Git.

## License

License information will be added when the project license is selected.
