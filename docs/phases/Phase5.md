You are building Phase 5 of Sol-O on top of a working Phase 4. Sol-O is a solar-system AI orchestration backend (FastAPI + plain Python, managed with uv). The Sun (sun.py) runs agents in order and passes outputs between them. Phase 5 makes it fail safely and makes every failure visible.

Work in the exact order below. Finish and test each part before the next. Do not ask me questions; use the defaults given. Output complete, working code for every file, never partial snippets.

## Hard rules
- Plain Python only. No LangChain, CrewAI or other agent frameworks.
- Do not break Phase 1-4 behavior. If you must change an existing file, say which and why.
- Never hardcode keys, model names or limits. All limits come from .env with the defaults below.
- Nothing irreversible (sending a Telegram message, email, or any external action) may happen without human approval, unless AUTO_APPROVE=true.
- No bare "except: pass". Every failure must be logged and turned into a clear, structured result.

## 5.1 Config (.env, with defaults in .env.example)
LLM_TIMEOUT_SECONDS=30
LLM_MAX_RETRIES=2
TOOL_TIMEOUT_SECONDS=15
RUN_TIMEOUT_SECONDS=180
MAX_AGENTS=7
REQUIRE_APPROVAL=true
AUTO_APPROVE=false
Create config.py that loads these once and exposes them. Replace any hardcoded values in existing code with it.

## 5.2 Retries and timeouts
- Every LLM call: timeout LLM_TIMEOUT_SECONDS, up to LLM_MAX_RETRIES retries with exponential backoff (1s, 2s, 4s). Retry only on timeouts, connection errors, HTTP 429 and 5xx. Do NOT retry on 401/403/400; fail immediately with a clear message.
- Every tool call (search, Telegram): timeout TOOL_TIMEOUT_SECONDS, one retry on timeout or 5xx.
- The whole run has a RUN_TIMEOUT_SECONDS budget. If exceeded, stop and finish with status "failed", reason "run_timeout", and keep the outputs collected so far.
- Create reliability.py holding the retry/backoff helper and the timeout logic so all code reuses it.

## 5.3 Failure states for bad JSON
- Keep the existing validate-and-retry loop (feed the validation error back to the model, max LLM_MAX_RETRIES).
- If an agent still returns invalid JSON or fails its Pydantic model, do NOT crash. Record a structured failure: {"agent", "error_type": "invalid_json"|"schema_mismatch"|"timeout"|"llm_error"|"tool_error", "message", "raw_output" (truncated to 2000 chars), "attempts"}.
- The Sun stops the chain at the first hard failure and returns {"status": "failed", "failed_at": "<agent>", "failure": {...}, "outputs": [...so far...]}.
- Error types must be an Enum in schemas.py.

## 5.4 Logging of every agent call
- Create a SQLite table agent_calls in data/sol-o.db: id, run_id, step, agent, started_at, duration_ms, attempts, status ("ok"|"failed"), error_type, error_message, input_chars, output_chars, raw_output (truncated to 2000 chars). One row per agent call, written even on failure.
- Also write tool calls to a table tool_calls: id, run_id, tool, started_at, duration_ms, status, error_message.
- Use Python's logging module (not print) for console logs with timestamps: INFO for normal events, WARNING for retries, ERROR for failures. Keep the existing emit() events working.
- Add endpoints:
  - GET /runs/{run_id}: the run plus all its agent_calls and tool_calls.
  - GET /runs?status=failed: filter runs by status.
- Add a CLI: uv run python sun.py --show <run_id> prints a readable timeline of that run (step, agent, duration, status, error).

## 5.5 Human approval before irreversible actions
- Irreversible steps (right now: the Telegram notifier, and any future email send) become a pending action instead of executing.
- When the chain reaches such a step and REQUIRE_APPROVAL=true and AUTO_APPROVE=false:
  1. Save a row in a new table pending_actions: id, run_id, action_type, payload_json, status ("pending"|"approved"|"rejected"|"expired"), created_at, decided_at.
  2. Mark the run status "awaiting_approval" and stop cleanly. Do not execute the action.
  3. Print/return the exact payload a human would be approving (the draft, the checker verdict, the issues).
- Approval methods:
  - CLI: uv run python sun.py --pending (list pending), --approve <id>, --reject <id>.
  - API: GET /pending, POST /pending/{id}/approve, POST /pending/{id}/reject.
- Approving executes the saved payload exactly as stored (no re-running the agents) and marks the run "complete". Rejecting marks it "rejected" and executes nothing.
- Pending actions expire after 24 hours and cannot be approved after expiry.
- An action can only be approved once. A second approve returns a clear error.
- DRY_RUN=true must still work: approving prints the message instead of sending it.

## 5.6 Safety nets
- Cap input size: if the task text is over 8000 characters, truncate it and add a warning to the run record.
- Never include API keys or tokens in logs, errors or stored payloads. Add a redact() helper and apply it to every log line and stored error message.
- If the checker returns "needs_changes" after the one allowed revision, the run finishes "partial" and still requires approval before any sending.

## 5.7 Tests (run them and show the real output of each)
1. Break the key on purpose (wrong API_KEY). Pass: fails immediately with error_type "llm_error", no retries on 401, clear message, run logged as failed.
2. Set LLM_TIMEOUT_SECONDS=1 with a slow prompt. Pass: retries with backoff, then fails with error_type "timeout", chain stops, failure recorded.
3. Mock an agent that returns invalid JSON every time (use a test flag or a fake LLM function in tests/). Pass: retries, then returns the structured failure with raw_output, no crash.
4. Normal run with REQUIRE_APPROVAL=true: uv run python sun.py "A customer asks: do you deliver on weekends?" Pass: the run ends "awaiting_approval", nothing is sent, a pending action exists.
5. uv run python sun.py --pending, then --approve <id>. Pass: the action executes once (printed if DRY_RUN=true) and the run becomes "complete". Approving the same id again returns an error.
6. Reject path: --reject <id>. Pass: nothing executes, run becomes "rejected".
7. uv run python sun.py --show <run_id>. Pass: a readable timeline with every agent call, durations and statuses, including the failed ones from tests 1-3.
8. Remove TAVILY_API_KEY. Pass: still works, claims marked unverified, the tool failure is logged in tool_calls.
9. Grep the logs and the database for any real API key or token. Pass: none found.
Write the failure tests as pytest tests in tests/ so I can re-run them with uv run pytest.

## Update docs
Update README.md (max 25 lines): new .env variables, approval commands, how to read a failed run, and the test commands.

## Finish with
- Every file created or changed.
- Pass/fail for each test above.
- Anything not completed and why.
- The exact commands to run everything.