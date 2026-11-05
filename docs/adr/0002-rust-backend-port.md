# ADR 0002: The backend is Rust; the Python tree stays as a frozen reference

- Status: accepted
- Date: 2026-11-05

## Context

The backend is 4,390 lines of Python: `app/` (3,694) on FastAPI, pydantic, `sqlite3`,
the `openai` SDK and `httpx`, plus `tools/` (696) holding the handlers the Sun executes.
It exposes 11 routes, one SSE stream, and 5 SQLite tables; prompts (`agents/*.md`) are
read from disk on every request and the UI is a single static `web/index.html`. There is
no Dockerfile and no CI; the service only runs from the checked-in `.venv`, and
`app/llm.py` reads `.env` once at import time, so a config change needs a restart.

The run is sequential by contract and is >99% waiting on the LLM provider. Porting to
Rust therefore buys no speed; the driver is deployment: one binary, no venv, no
import-time environment snapshot.

## Decision

Rewrite the backend in Rust and keep the Python tree in the repo, frozen.

- **Shape.** Root `Cargo.toml` + `src/`, one binary `sol-o`. `axum` + `tokio`; the
  orchestration is a sequential `async fn` per run; `rusqlite` (bundled SQLite) calls run
  in `spawn_blocking`; SSE fan-out is `tokio::sync::broadcast`; `reqwest` for the provider,
  Tavily and Telegram; `serde` + `schemars` for the contracts; `clap` for the CLI.
- **Scope.** `app/` + `tools/` + the operator CLI move. `agents/*.md`, `kb/*.md` and
  `web/index.html` stay on disk and are read at runtime.
- **Order.** Bottom-up, one reviewable stage at a time: schemas/reliability/config →
  llm → agents/toolbox → logger/events → orchestrator → approvals → main/cli. Rust listens
  on :8001 while the Python service keeps :8000.
- **Parity.** Semantic, not byte-for-byte. Preserved exactly: the SQLite schema and
  `_ensure_columns` migrations (an existing `data/sol-o.db` keeps working), event names,
  fields and `seq` numbering, the `GET /runs/{id}` payload shape, ISO timestamp formats,
  codepoint-based truncation limits (`MAX_RESULT_CHARS` and friends), the
  `json.dumps(default=str)` fallback, and `calc`'s Python arithmetic (`//` floors, `%`
  takes the dividend's sign, int/float promotion, `round(v, 6)`).
  Two accepted deviations: the validation-error text fed back to the model becomes
  Rust-shaped, and log lines become `tracing` fields instead of `INFO:sol_o.sun:…`.
- **Python's status.** Frozen reference implementation. No new features, no contract
  changes; `AGENTS.md` says so at cutover. It rots from the first Rust-only change, so it
  is a reading aid, not a fallback.
- **Tests.** Pure-logic suites port to Rust; the suites that mock Python internals are
  replaced by black-box integration tests against the binary; a throwaway
  `golden_diff` harness replays runs through both servers and is deleted at cutover.

## Why

- A single statically-linked binary (`rusqlite` bundled, `rustls`) removes the venv and
  the import-time `.env` read, which are the only two things that actually bite.
- The port target is ordinary orchestration code: no framework magic, no ORM, no plugin
  loader, one provider POST. The language is not where the complexity lives.
- Keeping the schema and events fixed means the browser UI is untouched and existing runs
  stay readable, which is what makes a rewrite verifiable rather than hopeful.

## Considered alternatives

- **Keep Python, add a Dockerfile.** Rejected as the whole answer: it fixes packaging but
  leaves the venv-only runtime and the import-time environment snapshot, and the unused
  `langchain` deps in `pyproject.toml` stay.
- **`sqlx` instead of `rusqlite`.** Rejected: SQLite is a local file, so an async pool buys
  nothing, and `sqlx`'s query macros want a live `DATABASE_URL` at build time.
- **Run both backends live.** Rejected: the researcher contract is already duplicated in
  three places; a second live backend makes it four.

## Consequences

- Retry quality after a schema mismatch may shift, because the model now sees a different
  error string.
- `calc` semantics and codepoint truncation must be reimplemented deliberately; inheriting
  Rust defaults (`f64::rem_euclid`, `str::len`) would silently change tool output.
- Cutover requires a clean golden diff and the UI's run/replay/approve/reject flows at
  375/768/1440px against :8001 before the Rust service takes :8000.
