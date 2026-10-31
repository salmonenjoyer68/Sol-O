# Phase 2: The Sun (Orchestrator)

Build Phase 2 of Sol-O on top of the finished Phase 1 (FastAPI + one agent + strict JSON). Add an orchestrator called "the Sun" that runs 2-3 agents in order and passes outputs between them. Give me complete, working code for every file, not snippets. Do not break or rewrite Phase 1 files unless needed; if you must change one, say which and why.

## Concept
Sol-O is a solar system. The Sun (orchestrator) receives a task, decides which agents (planets) to call and in what order, and passes each agent's output to the next. Max 7 agents per run. Goal: automate repetitive business tasks.

## Files to create or change
```
sol-o/
├── sun.py               (NEW: orchestrator, runnable from terminal)
├── agents/
│   ├── researcher.md    (exists)
│   ├── writer.md        (NEW)
│   ├── checker.md       (NEW)
│   └── sun.md           (NEW: planner prompt, used in step 2)
├── main.py              (add POST /orchestrate)
├── schemas.py           (add orchestration models)
└── README.md            (add Phase 2 run steps, max 10 lines)
```

## The loop (sun.py must follow this exactly)
```
1. RECEIVE task
2. PLAN: list of steps [{agent, task}], max 7
         (hardcoded first, LLM-planned second)
3. FOR each step:
     a. emit "agent_called"
     b. build input = original task + outputs of earlier agents
     c. run the agent (load agents/{name}.md, call LLM, validate JSON, retry up to 2x with the error fed back)
     d. emit "agent_done"
     e. save output to memory
     f. if status is needs_clarification or the agent failed -> STOP and report
4. COMPILE final result with all outputs
5. emit "all_done"
```

## Requirements
1. **Hardcoded flow first.** Default plan is researcher -> writer -> checker. The task text is inserted into each step.
2. **LLM planner second.** Add `plan_with_llm(task)` using `agents/sun.md` as the system prompt. It receives the task plus the list of available agents (read from the `agents/` folder, excluding `sun.md`) and returns JSON `{"steps": [{"agent": "...", "task": "..."}]}`. Drop any step naming an agent that doesn't exist. Enforce max 7 steps. Switch with a `--plan` CLI flag.
3. **Reuse Phase 1 code.** Use the existing LLM wrapper in `llm.py` and the fence-stripping and validation logic. Do not duplicate it.
4. **Pass outputs forward.** Each agent's message contains: the original task, its own step instruction, and the JSON outputs of all earlier agents.
5. **Events.** A single `emit(event, **data)` function. For now it prints a line like `[agent_called] {"step": 1, "agent": "researcher"}`. Events: `plan_ready`, `agent_called`, `agent_done`, `agent_failed`, `stopped`, `all_done`. Phase 6 will replace print with SSE/WebSocket, so keep all event output going through this one function.
6. **Stop conditions.** Stop the run if an agent returns `needs_clarification` or fails after retries. Return a result saying where it stopped and include the outputs collected so far.
7. **Result shape.** `{"status": "complete" | "failed" | "needs_clarification", "outputs": [{"agent": "...", "output": {...}}], ...}`.
8. **API.** Add `POST /orchestrate` accepting `{"task": "...", "use_llm_planner": false}` and returning the result. Keep the existing `/run` endpoint working.
9. **Terminal.** `uv run python sun.py "task"` runs the hardcoded flow. `uv run python sun.py "task" --plan` runs the LLM planner. Print events live, then the final JSON.
10. **Logging.** Log per agent: name, attempts, duration. Log total run time.
11. **Timeouts.** Reuse the Phase 1 30-second timeout per LLM call.

## Agent prompts to write
All agents return ONLY valid JSON with at least `agent` and `status` fields (`complete` | `partial` | `needs_clarification`). Same rules as the Researcher: no fabrication, stay in scope, ask for clarification if the task is too vague.

- **writer.md**: Takes the task and earlier research. Writes the deliverable (email, reply, summary, or report) using ONLY facts present in earlier outputs. Never adds new factual claims. Output: `{agent, status, draft, facts_used[], notes, clarification_needed}`.
- **checker.md**: Reviews the writer's draft against the researcher's findings. Flags unsupported claims, errors, and tone issues. Output: `{agent, status, verdict: "approved"|"needs_changes", issues[{problem, location, suggestion}], clarification_needed}`.
- **sun.md**: Planner prompt. Given a task and available agents, choose the minimum agents needed, in order, with a clear instruction for each. Max 7. Return ONLY `{"steps": [{"agent": "...", "task": "..."}]}`. Never invent agents.

## Do NOT
- Add a database, auth, streaming, or a frontend.
- Use LangChain, CrewAI, or any agent framework. Plain Python only.
- Run agents in parallel. Sequential only in this phase.
- Hardcode keys or model names.
- Let the Sun answer the task itself. It only plans and routes.

## Done when
- `uv run uvicorn main:app --reload` starts without errors.
- `uv run python sun.py "Explain why small cafes should use online ordering"` runs researcher -> writer -> checker and prints all events plus a final JSON result.
- The same task with `--plan` runs a valid LLM-chosen plan of 1-7 known agents.
- A vague task (e.g. "do the thing") stops cleanly with `needs_clarification` instead of crashing.
- `POST /orchestrate` returns the same result as the terminal run.

Finish by listing the exact terminal commands to run and test everything.