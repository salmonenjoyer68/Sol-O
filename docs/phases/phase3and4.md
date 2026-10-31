You are building Phase 3 and Phase 4 of Sol-O on top of a working Phase 2. Sol-O is a solar-system AI orchestration backend (FastAPI + plain Python, managed with uv). The Sun (orchestrator, sun.py) runs agents ("planets") in order and passes outputs between them. Goal: automate one repetitive business task end to end.

Work in the exact order below. Finish and test each part before starting the next. Do not ask me questions; use the defaults given. Output complete, working code for every file, never partial snippets.

## The business task to automate (default)
Customer inquiry handling:
  customer message in -> Researcher gathers facts -> Writer drafts a reply -> Checker reviews it -> Logger saves everything -> Notifier sends the result to me on Telegram.
If I have set TASK_DESCRIPTION in .env, use that instead for the sample task text.

## Hard rules
- Plain Python only. No LangChain, CrewAI or other agent frameworks.
- Do not break existing Phase 1 and Phase 2 files. If you must change one, say which and why.
- LLM calls stay provider-agnostic: BASE_URL, API_KEY, MODEL come from .env (OpenAI-compatible API). Never hardcode keys or model names.
- Tools are run by Python code, NOT by the model through function calling. The code calls the tool, then puts the result into the agent's message. This is more reliable.
- Every agent returns ONLY valid JSON with at least "agent" and "status" ("complete" | "partial" | "needs_clarification"). Reuse the existing retry/validation logic in the Sun.
- Every new agent gets its own prompt file in agents/ and its own Pydantic model in schemas.py.
- Agents never fabricate facts, URLs or quotes. Stay in scope.
- Max 7 agents per run still applies.

## PHASE 3: More planets

### 3.1 Agents (add one at a time, test each alone before moving on)
1. researcher.md: already exists. Keep it.
2. writer.md: writes the customer reply using ONLY facts from earlier outputs. Output: {agent, status, draft, facts_used[], notes, clarification_needed}.
3. checker.md: reviews the draft against the researcher's findings. Output: {agent, status, verdict: "approved"|"needs_changes", issues[{problem, location, suggestion}], clarification_needed}.
4. logger: this one is mostly CODE, not an LLM call. Create logger.py that saves each run to SQLite (file: data/sol-o.db). Table runs(id, created_at, task, status, final_draft, verdict, outputs_json, duration_seconds). The Sun calls it as the last step of every run, success or fail. Use the standard sqlite3 module.

### 3.2 Checker loop
If the checker returns verdict "needs_changes", the Sun sends the draft plus the issues back to the writer ONE time, then runs the checker again. If it still fails, finish with status "partial" and include the issues. Never loop more than once.

### 3.3 Update the plans
- Hardcoded plan becomes: researcher -> writer -> checker (with the revision rule) -> logger.
- Update agents/sun.md so the LLM planner knows about writer, checker and logger, and that logger is always last.

### 3.4 Phase 3 tests (run them and show real output)
1. uv run python sun.py "A customer asks: do you deliver on weekends and what are your prices?"
   - Pass: researcher, writer, checker, logger all run; a row appears in data/sol-o.db.
2. Run the same with --plan. Pass: valid plan, runs, row saved.
3. uv run python sun.py "do the thing". Pass: stops with needs_clarification, still logged, no crash.
4. Add GET /runs (last 20 runs from SQLite). Pass: returns the rows as JSON.

## PHASE 4: Real tools

### 4.1 Search for the Researcher (Tavily)
- Create tools/search.py with search(query, max_results=5) using the Tavily API over httpx. Key from TAVILY_API_KEY in .env.
- Before the researcher runs, the Sun calls search() with a query derived from the task, then adds the results (title, url, content snippet) to the researcher's message.
- Update researcher.md: remove the "no search tool" line; say it must use ONLY the provided search results, cite each source_url from them, and mark anything else "unverified".
- If TAVILY_API_KEY is missing or the search fails, continue without results and mark all claims unverified. Do not crash.

### 4.2 Telegram notifier
- Create tools/telegram.py with send_message(text) using the Telegram Bot API over httpx. TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID from .env.
- Add a "notifier" step at the end (after logger). It sends: the task, the final draft, the checker verdict, and any issues, formatted short and readable. Split messages over 4000 characters.
- Add DRY_RUN=true support in .env. When true, print the message instead of sending it. Default to true in .env.example.

### 4.3 Real input
- Add POST /inbound accepting {"message": "...", "source": "..."} and running the full orchestration on it in the background, returning {"run_id": ...} immediately.
- Add terminal support: uv run python sun.py --file inquiry.txt reads the task from a text file.

### 4.4 Phase 4 tests (run them and show real output)
1. With DRY_RUN=true: uv run python sun.py --file inquiry.txt. Pass: search results appear in the researcher's findings with real source URLs, the full chain runs, the Telegram message is printed, and a row is logged.
2. With DRY_RUN=false and real Telegram keys: Pass: the message arrives on my Telegram. If keys are missing, say so and skip this test.
3. Remove TAVILY_API_KEY temporarily. Pass: run completes with all claims "unverified", no crash.
4. POST /inbound with a sample message. Pass: returns a run_id and the run appears in GET /runs.

## Update docs
Update README.md (max 20 lines): setup, the .env variables (add TAVILY_API_KEY, TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID, DRY_RUN, TASK_DESCRIPTION), and the test commands.

## Finish with
- Every file created or changed.
- Pass/fail for each test above.
- Anything not completed and why.
- The exact commands to run everything.