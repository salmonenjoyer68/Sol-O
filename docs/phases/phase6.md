You are building Phase 6 of Sol-O on top of a working Phase 5. Sol-O is a solar-system AI orchestration system (FastAPI + plain Python, managed with uv). The Sun (sun.py) is the orchestrator, and each agent is a planet. Phase 6 streams real run events to a browser and shows them as a live solar-system animation.

Work in the exact order below. Finish and test each part before the next. Do not ask me questions; use the defaults given. Output complete, working code for every file, never partial snippets.

## Hard rules
- Backend: plain Python, no new frameworks. SSE only (no WebSocket), using FastAPI's StreamingResponse.
- Frontend: ONE self-contained file, web/index.html. Vanilla JS + HTML5 Canvas. No React, no build step, no npm, no CDN libraries. Served by FastAPI at GET /.
- The animation must be driven ONLY by real events from the backend. No fake timers pretending an agent is working. The only allowed local animation is cosmetic (orbiting, glow pulse, star twinkle).
- Do not break Phase 1-5 behavior. If you must change an existing file, say which and why.
- If web/ already contains my earlier solar-system canvas prototype, reuse its look and code as the base instead of starting over. If not, build from the design spec below.

## 6.1 Backend: event bus
- Create events.py with an in-memory event bus. Every event is a dict: {"run_id", "seq" (increasing per run), "ts" (ISO), "event", "data"}.
- Route the existing emit() function through this bus (keep the console logging too). Save every event to a new SQLite table run_events(id, run_id, seq, ts, event, data_json) so runs can be replayed later.
- Events to emit, in this meaning:
  - run_started {task}
  - plan_ready {agents: [names in order]}
  - agent_called {step, agent}   -> planet appears
  - agent_working {step, agent, attempt}   -> planet glows (emit at the start of each LLM attempt and each tool call)
  - agent_done {step, agent, status, summary (max 140 chars)}
  - agent_failed {step, agent, error_type, message}
  - awaiting_approval {pending_id, preview}
  - approval_decided {pending_id, decision}
  - all_done {status, seconds}   -> planets align
  - run_failed {failed_at, error_type, message}
- Every event must come from real code paths in the Sun, never invented.

## 6.2 Backend: SSE endpoints
- GET /stream: live SSE stream of events for all runs. Send a keepalive comment every 15 seconds. Support a Last-Event-ID header to resume. Multiple browser tabs must work at once.
- GET /runs/{run_id}/events: returns the saved events for a past run as JSON (for replay).
- POST /orchestrate must start the run in the background and return {"run_id"} immediately, so the browser can watch it live.
- Add CORS for localhost only.
- Serve web/index.html at GET /.

## 6.3 Frontend: visual design (make it look like a real solar system, dark and cinematic)
Layout (flat, no nested panels):
- Full-screen canvas. Deep space background (near-black with a subtle blue-violet gradient), 150-250 twinkling stars, a faint nebula glow. Handle devicePixelRatio and window resize.
- The Sun at the center: a glowing orb with a layered radial-gradient corona, a slow pulsing glow, and soft rays. Label "SUN" and, under it, the current status text (idle / planning / orchestrating / complete / failed).
- Orbit rings: thin, low-opacity ellipses around the Sun, one per planned agent (up to 7), each at a different radius.
- A slim top bar with: task input, a Run button, a status pill, and a replay dropdown listing past runs. A small collapsible event log at the bottom-left showing the last 8 events as one line each.
- Camera: slight tilt (use ellipses with a y-scale of about 0.55) so it feels 3D, no actual 3D library.

Planets (one per agent, fixed colors and sizes):
- researcher: cyan-blue, medium
- writer: warm orange, medium
- checker: green, small
- logger: purple, small
- notifier: pink, small
- any other agent: a color picked from a fixed palette by name hash
- Each planet has a soft glow, a subtle surface gradient, a name label, and (for some) a small ring or moon so they feel distinct.

Behavior driven by events:
- plan_ready: draw the faint empty orbit rings and dim placeholder dots for the planned agents. The Sun status becomes "planning".
- agent_called: the planet fades and scales in on its orbit with a short light-trail from the Sun to the planet. It starts orbiting slowly.
- agent_working: the planet glows brighter, pulses, and a thin energy beam runs from the Sun to it. Show a small "attempt N" badge when attempt > 1.
- agent_done: the beam fades, the glow settles to a steady soft ring, and a small check mark appears. Show the summary as a small tooltip on hover.
- agent_failed: the planet turns red, flickers, and shows an X. Pause all other orbits slightly. The Sun status becomes "failed" with the error_type.
- awaiting_approval: the Sun shows a gold pulsing ring and the text "waiting for approval". Show Approve and Reject buttons that call POST /pending/{id}/approve and /reject. When approval_decided arrives, continue or end the animation accordingly.
- all_done (status complete): every finished planet smoothly eases onto one straight line (the alignment) through the Sun, holds for about 3 seconds with a bright golden flash along the line, then gently returns to normal orbits. The Sun status becomes "complete".
- run_failed or all_done with status failed/partial: no alignment. Show the failed planet in red and the status text.
- Clicking a planet opens a small panel with that agent's full output JSON, pulled from GET /runs/{run_id}.

Replay: choosing a past run in the dropdown fetches /runs/{run_id}/events and plays them back with the original relative timing (sped up so no gap exceeds 1.5 seconds), using the same rendering code as live mode. Mark the status pill "REPLAY" while replaying.

Performance and quality:
- Use requestAnimationFrame, keep 60 fps with 7 planets, and pause the loop when the tab