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

- **Shape.** Root `Cargo.toml` + `src/`, one binary `sol-o`. `axum` + `tokio`; each
  orchestration is a blocking `Orchestrator::run` call driven through `spawn_blocking`, so
  the sequential contract stays a plain function over sync SQLite calls; SSE fan-out is
  `tokio::sync::broadcast`; `reqwest` for the provider, Tavily and Telegram; `serde` plus
  hand-written JSON schemas for the contracts; `clap` for the CLI. Schemas are written out
  rather than derived (`schemars`) because the planner schema needs explicit mutations —
  the agent enum and the `max_agents` step cap are injected per request.
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
  Two deviations were accepted at cutover: the validation-error text fed back to the model
  becomes Rust-shaped, and log lines become `tracing` fields instead of `INFO:sol_o.sun:…`.
  The parity addendum below records three more, for five.
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

## Cutover (ticket #33)

Shipped. `sol-o serve` defaults to `127.0.0.1:8000`, the port the Python service used, so
the UI and every client keep their address. The golden diff ran one scripted-provider run
through both services over HTTP: same run detail, same 23 events in the same order with the
same fields, the same approval payload and the same post-approval status, once ids,
timestamps and durations are excluded. Both backends store the validated output
re-serialized in `agent_calls[].raw_output` (`run.output.model_dump_json()` in Python,
`serde_json::to_string` here); the only difference is JSON member order, because the
reference keeps pydantic field order and `serde_json` orders map keys. The UI's run,
replay, approve and reject flows were verified against the Rust service at 375, 768 and
1440px. `app/`, `tools/` and `tests/` are frozen reference code: nothing builds, tests or
runs them any more, and no virtualenv is needed for the backend. The throwaway replay
harness was never committed, so there was nothing to delete.

## Parity decisions (ticket #40)

- Status: accepted
- Date: 2026-11-07

Four decisions from the post-cutover review; each one was checked against the merged code.

- **`POST /run` writes no `agent_calls` row.** Parity beats issue #25's "one agent-call row
  per call": the reference's handler only logs (`app/agents.py`), and `POST /run` creates
  no `runs` row for an audit row to attach to. The orchestrated path still writes one row
  per agent call.
- **An out-of-range `GET /runs?limit=` keeps the 422.** The reference rejects it through
  `Query(20, ge=1, le=100)`; the port returns the same status, and `parse_limit` in
  `src/http.rs` answers with the port's `{"error": "invalid_request", "detail": "limit
  must be between 1 and 100"}` body rather than FastAPI's `{"detail": [...]}` list. The
  status is parity; the body is one of the deviations below.
- **`fetch_url` decodes page text as UTF-8 only.** `extract_text` uses
  `String::from_utf8_lossy` and ignores the declared `Content-Type` charset that the
  reference honours. It is a labelled `shortcut:` in `src/fetch.rs`; upgrade if non-UTF-8
  pages matter.
- **`calc` fails outside its integer range.** Non-finite results fail with the reference's
  `result is not a finite number`; integer `+ - * ** // %` and unary negation are checked
  over the 128-bit signed range, and an out-of-range result fails with `integer result out
  of range` instead of wrapping or panicking. Python's integers are arbitrary-precision,
  so this is a deliberate deviation, chosen over a bignum dependency.

**Accepted deviations: five, not two** — the two from the cutover (Rust-shaped
validation-error text, `tracing` log lines) plus UTF-8-only `fetch_url` decoding, the
checked-integer `calc` range, and the `invalid_request` body on an out-of-range `limit`.
