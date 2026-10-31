Build Phase 1 of Sol-O, a multi-agent orchestration backend: a FastAPI app that runs ONE agent and returns strict JSON. Give me complete, working code for every file, not snippets.

## Project structure
sol-o/
├── agents/researcher.md   (system prompt, loaded from file)
├── main.py                (FastAPI app)
├── llm.py                 (LLM call wrapper)
├── schemas.py             (Pydantic models)
├── test_run.py            (terminal test client)
├── requirements.txt
├── .env.example
└── README.md              (run steps, 10 lines max)

## Requirements
1. POST /run accepts {"agent": "researcher", "task": "<string>"} and returns the agent's JSON output.
2. Load the system prompt from agents/{agent}.md at request time, so edits apply without a restart. Return 404 if the agent file doesn't exist.
3. llm.py uses the OpenAI-compatible chat completions API via the `openai` package, with BASE_URL, API_KEY and MODEL read from .env. This lets me swap between Groq, OpenRouter, Gemini or Ollama without code changes.
4. Force JSON output: use response_format json_object if supported, and also tell the model in the prompt to return JSON only.
5. Validate the model output against a Pydantic model in schemas.py:
   {agent, status ("complete"|"partial"|"needs_clarification"), task, summary, findings[{claim, source_url, confidence}], conflicts[], gaps[], clarification_needed}
6. If the output is not valid JSON or fails validation: retry up to 2 times, feeding the validation error back to the model. If it still fails, return HTTP 502 with {"error": "invalid_agent_output", "raw": "<raw output>"}.
7. Strip markdown code fences (```json) before parsing, since some models add them.
8. Set a 30-second timeout on the LLM call. Return HTTP 504 on timeout.
9. Log each call (agent, task, attempts, duration) to the console.
10. There is no search tool in Phase 1. Add this line to the end of researcher.md: "No search tool is available yet. Use only the task text; mark all claims unverified and list what you could not check in gaps."
11. test_run.py: posts a sample task to /run, prints the JSON, and exits non-zero if the response is invalid. It must accept the task as a command-line argument.

## Do NOT
- Add an orchestrator, multiple agents, a database, auth, streaming or a frontend.
- Use LangChain or any agent framework. Plain Python only.
- Hardcode keys or model names.

## Done when
- `uvicorn main:app --reload` starts without errors.
- `python test_run.py "your task here"` prints valid JSON matching the schema.
- Running it 10 times in a row gives valid JSON every time (or a clean 502 with the raw output).

Finish by listing the exact terminal commands to install, configure .env, run, and test.