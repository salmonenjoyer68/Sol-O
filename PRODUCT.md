# Sol-O

`register: product`

## What it is

Sol-O turns a task into a live multi-agent run. A Sun plans the work, planets — one per step, including the logger and notifier system steps — research, write, check, log, and notify, and, when the run requires approval, it stops at a blocking gate before anything is sent. The browser console is where that run is started, watched, inspected, and decided.

The console is one full-viewport page: a header, an activity feed in the left rail, the stage in the middle, a planet roster in the right rail, and a command dock at the bottom. There is no hero, no "how it works", and no footer.

## Users

- The developer or operator running Sol-O on a laptop, often while screen-sharing or demoing to someone else.
- The reviewer who needs to see what each planet did, how long it took, and what the final draft is.
- The maintainer replaying a stored run to understand a failure or an approval decision.

They are usually in a dim room, looking at one screen, with the run as the centre of attention.

## Product purpose

Make a multi-agent run legible while it is happening and trustworthy when it stops. The solar system is the model, not decoration: the Sun plans, planets are steps, orbits show progress, and the alignment marks completion. The console should make the run's state, plan, activity, and outcome readable without opening developer tools, and it should keep working when the run is replayed from stored events.

## Brand

An instrument, not a toy. Calm, precise, and observational. Warm graphite, bone text, solar amber, and the planets' own colours. The page should feel like an orrery on a workbench: measured, physical, and quiet until the run moves.

## Tone

Direct and factual. Name the state, the step, and the outcome. Run states are written as words (`waiting for approval`, not `awaiting_approval`), and an approval decision is never glued onto a run state. No marketing voice inside the console, no exclamation marks, no anthropomorphic chatter. Empty states explain what will appear ("the Sun's plan appears here"), not why the product is great.

## Anti-references

- Glassmorphic space dashboards: blurred panels, neon navy, floating cards, decorative glow.
- Hero-first SaaS landing pages inside the tool: a headline, a subheadline, feature cards, and the product somewhere below.
- Identical card grids and nested cards.
- Gradient text, side-stripe borders, and the big-number hero-metric template.
- A dashboard that hides the live run behind chrome.
- A second status vocabulary: raw identifiers (`awaiting_approval`) or a run state concatenated with an approval decision (`needs_clarification · approved`).

## Principles

1. **The run is the page.** One header, one stage, two rails, one dock. The stage, the plan, and the outcome own the viewport; marketing copy does not appear at any width.
2. **One state, one vocabulary.** The status pill, the Sun's first line, and the result meta all use the same human-readable run-state labels. An approval decision is a separate detail line, not part of the state.
3. **The alignment is the payoff.** On desktop the result opens in the left rail, so nothing covers the stage when the planets line up. Below 1180px the result becomes a sheet only after the alignment finishes, so the chrome steps aside without hiding the payoff.
4. **Planets are steps.** The roster counts steps, not agent names: one row per step with its number — planned, system, and revision steps alike — so a checker revision can show the same agent twice without ambiguity.
5. **Approval is explicit.** The gate is centred and blocking, says that nothing has been sent and the decision is final, cannot be dismissed by Escape or a stray click, and a replayed gate is read-only with no decisions.
6. **Quiet surfaces, loud planets.** Chrome is matte: 2px radii, hairline borders, no blur, no glass. Colour belongs to the planets and the Sun. Run is the only solid accent surface; Approve is the only other solid fill, in the success colour.
7. **Readable first.** Reading text is 14px (12–13px in the drawer and approval preview), labels are 10–12px, contrast clears AA, and every step is reachable by keyboard and screen reader through the roster, not the canvas.
8. **One file, no build.** The UI stays a single static document with vanilla JavaScript and one canvas, one external font stylesheet, and no bundler, CDN, or new runtime dependency.
