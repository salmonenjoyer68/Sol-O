# Sol-O web demo: minimalist redesign

`status: accepted`  
`scope: web/index.html`  
`register: product`

## Direction

Build a **quiet orrery**: a precise run console with a flat orbital diagram at its centre and almost no decorative chrome. The Sun, planets, and final alignment carry the product identity. Everything else supports reading and operating the run.

This is a subtraction pass, not a new product layout. Keep the current header, activity rail, central stage, step roster, result surface, replay controls, approval gate, and command dock. Remove visual noise and redundant treatment before removing information.

## Product context

Sol-O turns a task into a live multi-agent run. The Sun plans the work; one planet represents each step, including system and revision steps. The person running the demo needs to start a task, understand the current state, inspect a step, approve or reject a pending notification, and read the final result.

The primary scene is a developer screen-sharing a live run on a laptop. The interface must remain understandable without explanation and must not compete with the presenter's narration.

## Goals

- Make the current run state understandable in one glance.
- Make the plan and the active step easier to scan than the activity feed.
- Give the final result a quiet, readable surface.
- Preserve the solar-system metaphor without decorative space imagery.
- Keep one unmistakable primary action: **Run**.
- Preserve keyboard access, replay, responsive behavior, and the blocking approval gate.

## Non-goals

- No marketing hero, feature sections, onboarding carousel, or landing-page copy.
- No backend, route, event, payload, or persistence changes.
- No new framework, build step, icon library, font request, or runtime dependency.
- No change to run-state vocabulary or approval semantics.
- No light-theme conversion for its own sake; the existing warm graphite identity remains.

## Design principles

1. **The orbit is the identity.** Spend visual expression on the stage. Keep every other surface plain.
2. **State before telemetry.** Run state, active step, and required decisions outrank timestamps and event details.
3. **One treatment per meaning.** Agent colour identifies an agent. Semantic colour identifies a state. Neither is decoration.
4. **Structure replaces containers.** Use spacing and a small number of dividers instead of chips, nested panels, shadows, and repeated outlines.
5. **Motion explains change.** A planet may move when work advances; the alignment marks completion. Ambient motion goes away.
6. **Removal must not hide meaning.** Keep the roster as the accessible equivalent of the canvas and retain all decision and failure details.

## Visual system

### Colour

Keep the established warm graphite, bone, and solar amber palette. Reduce how much of the screen uses colour rather than introducing a new palette.

| Token | Value | Use |
| --- | --- | --- |
| `--bg` | `#0c0b09` | Entire page and stage; no background illustration |
| `--surface` | `#13110e` | Header, rails, dock, drawer, result, and approval surface |
| `--fg` | `#ece7dc` | Primary reading text and active controls |
| `--muted` | `#a39a89` | Secondary text, timestamps, and inactive step numbers |
| `--line` | `#2b2720` | The few structural dividers and orbit rings |
| `--accent` | `#e9b45c` | Sun, Run, keyboard focus, and the current interactive target |

Retain the existing semantic colours for success, warning, error, and planning. Use them for text and small status dots, not row fills or large badges. Retain agent colours only for planet discs and the small dot beside an agent name. Do not colour whole rows by agent.

The only solid accent fills are **Run** and **Approve and send**. No gradients, glass, glow, nebula, noise, or decorative shadow.

### Typography

Use the fonts already loaded by the page: **Space Grotesk** for reading and controls, **JetBrains Mono** only for timestamps, durations, run IDs, and raw output. Do not add a display face.

| Role | Specification |
| --- | --- |
| Body and result | 14px / 1.6, weight 400 |
| Wordmark | 15px / 1.2, weight 600, `Sol-O` as written |
| Stage state | 20px / 1.3, weight 500, sentence case |
| Agent names and primary controls | 13px / 1.4, weight 600 |
| Section headings | 12px / 1.4, weight 600, sentence case |
| Metadata and activity | 12px / 1.55, weight 400 |
| Raw output | 12px / 1.55, JetBrains Mono |

Use sentence case throughout. Remove uppercase transformations and wide tracking from labels, statuses, and buttons. Use tabular numerals for times, durations, and step counts. Keep result text within a comfortable measure of about 68 characters where the surface width allows it.

### Shape and spacing

- Keep the existing 2px radius for inputs and the approval surface; most structural regions have no radius.
- Use an 8px spacing base: 8px for compact control gaps, 12px inside rows, 16px around major surfaces, and 24px between unrelated content groups.
- Use dividers only between unlike regions or adjacent step rows. Remove decorative borders around statuses and presets.
- Use no shadows except one restrained shadow on the live approval surface if separation from the scrim requires it.

## Layout

### Desktop

Retain the current three-column workspace and its dimensions during the first pass. Minimalism should come from quieter content, not narrower reading surfaces.

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ Sol-O                 ● Running                         Replay a run…   │
├────────────────────────┬───────────────────────────────┬─────────────────┤
│ Activity               │                               │ Run plan        │
│                        │                               │ 2 of 5 complete │
│ 14:02:11  researcher   │          orbit rings          │                 │
│ Research completed     │              ●                │ 01 Researcher   │
│                        │            Sol-O              │    Done         │
│ 14:02:14  writer       │        Running step 2         │                 │
│ Draft in progress      │                               │ 02 Writer       │
│                        │       ○       ○       ○       │    Working      │
│                        │                               │                 │
│                        │                               │ 03 Checker      │
│                        │                               │    Waiting      │
│                        │                               │ …               │
├────────────────────────┴───────────────────────────────┴─────────────────┤
│ Plan a trip   Review code   Write a report                             │
│ [ Give the Sun a task…                                      ] [ Run ]  │
└──────────────────────────────────────────────────────────────────────────┘
```

Alignment rules:

- Left-align all reading and controls. Centre only the orbital scene and its short state caption.
- Keep the header to one line where space permits.
- Put the plan count in the roster heading; remove the separate dock counter.
- Let the stage occupy all remaining space. Do not add introductory copy to fill it.
- Keep the result in the left rail on desktop so it never covers the final alignment.

### Smaller screens

Retain the existing responsive model below 1180px: one stage column, a command dock, and a collapsible surface for **Run plan** and **Activity**. Result and step details open as full-screen reading surfaces after the relevant interaction or completion sequence.

At phone width:

```text
┌──────────────────────────────┐
│ Sol-O          ● Running     │
│ Replay a run…                │
├──────────────────────────────┤
│                              │
│             ●                │
│           Sol-O              │
│      Running step 2          │
│       ○     ○     ○          │
│                              │
├──────────────────────────────┤
│ Run plan and activity        │
├──────────────────────────────┤
│ [ Give the Sun a task…     ] │
│ [ Run                      ] │
└──────────────────────────────┘
```

Use at least 16px side gutters plus safe-area insets. Allow presets to wrap above the task input rather than scroll horizontally. No page-level horizontal scrolling.

## Component direction

### Header

Use the wordmark, a quiet run-state button, and replay controls. Replace the bordered, uppercase status pill with a small semantic dot and sentence-case label. Preserve the button behavior that reopens an available result. Show replay timing controls only during replay.

### Stage

The stage is the single expressive element:

- Solid `--bg`; remove stars, noise, nebula gradients, camera drift, and decorative rays.
- Draw thin, neutral orbit rings and simple filled planet discs in agent colours.
- Remove planet textures, moons, decorative ring bands, halos, and surface shading.
- Keep the Sun as a small amber disc with a restrained label and the current human-readable state beneath it.
- Keep canvas tooltips for pointer users and the roster as the keyboard and screen-reader equivalent.
- Preserve the final planet alignment as the only orchestrated animation.

An idle stage shows the Sun and `Ready for a task`. Do not add a headline or product explanation.

### Run plan

Rename the visible roster heading to **Run plan**. Keep one row per planned, system, or revision step. Each row contains:

1. The actual sequence number in muted mono.
2. A small agent-colour dot and the agent name.
3. A short role or task summary when available.
4. A sentence-case state: `Waiting`, `Called`, `Working`, `Done`, or `Failed`.

Remove the synthetic progress bars and working shimmer. They duplicate state and imply measured progress that the system does not have. The full row remains the button for opening step details, with visible keyboard focus.

### Activity

Keep the existing event stream and 40-line cap, but simplify each entry to a timestamp, a small status or agent dot, and a plain-language summary. Remove row background tints and uppercase headings. Permit summaries to wrap so failures and tool details are not silently clipped. Keep newest events at the bottom and preserve append-only live-region behavior.

### Command dock

Keep the three example tasks, the task input, and Run. Style examples as quiet text buttons rather than outlined chips. Keep Run as the single solid amber control. Move the plan count to **Run plan**. Preserve Enter-to-run and existing disabled or working behavior.

### Result

Keep the existing desktop placement and dialog semantics. Use a plain header, a small metadata group, readable body text, and one quiet Replay action. Present state, duration, verdict, and run ID as separate labelled values instead of an uppercase dot-joined string. Do not animate the result into view; its appearance is already a clear state change.

### Step details

Retain the drawer on desktop and full-screen surface on smaller screens. Use the same typography and spacing as the result. Keep role, task, duration, and raw output. Do not add tabs, accordions, or nested cards.

### Approval gate

Keep the live gate centred, modal, and blocking. Use one plain surface with:

- Heading: `Approval required`
- Explanation: `Nothing has been sent. Approving sends the stored message once. Rejecting ends this action.`
- The stored message preview
- `Reject` and `Approve and send`

Do not allow Escape or a scrim click to decide or dismiss it. A replayed gate remains read-only and non-modal. Approval decisions stay separate from run-state labels.

## Motion

- Use immediate state changes for text, dots, rows, and result visibility.
- Keep control feedback short, at most 120ms; avoid hover movement.
- Animate only planet travel needed to explain run progress and the existing completion alignment.
- Remove corona pulses, ray rotation, star twinkling, parallax, roster shimmer, and result entrance animation.
- Under `prefers-reduced-motion: reduce`, snap all state and alignment changes into place. Never delay access to the result or approval controls for animation.

## Copy rules

Use the established run-state vocabulary in sentence case: `Idle`, `Planning`, `Running`, `Waiting for approval`, `Complete`, `Partial`, `Needs clarification`, `Rejected`, and `Failed`.

Prefer direct labels:

| Instead of | Use |
| --- | --- |
| `PLANETS` | `Run plan` |
| `REPLAY · COMPLETE` | `Replay — Complete` |
| `Approve & send` | `Approve and send` |
| `No run yet — the Sun's plan appears here.` | `Start a run to see its plan.` |
| `No events yet — lines appear here when a run starts.` | `Activity appears when a run starts.` |

Do not add celebratory copy, exclamation marks, anthropomorphic status messages, or raw backend identifiers.

## Accessibility and resilience

- Keep all controls reachable by keyboard, including every roster row and the result reopen action.
- Retain visible amber focus outlines and existing dialog focus management.
- Do not encode agent or run state by colour alone; keep visible text labels.
- Verify text, control, and semantic-state contrast against WCAG AA during implementation.
- Keep touch targets at least 44px on phone-sized layouts, even when desktop controls are more compact.
- Preserve live-region behavior without re-announcing the full activity history.
- Keep the canvas decorative to assistive technology and the roster complete without it.
- Test long tasks, long agent output, duplicate agents in revision steps, empty runs, failures, and replay errors.

## Implementation boundaries

Implement the proposal in `web/index.html` only. Keep it self-contained with vanilla JavaScript, one canvas, and the existing Google Fonts stylesheet. Preserve existing DOM behavior, API calls, SSE handling, event names, replay logic, and storage contracts. Remove unused canvas code and CSS after the visual simplification rather than leaving dead branches behind.

After implementation, reconcile `DESIGN.md` with the accepted result and adjust `PRODUCT.md` only if a product-level principle actually changes. This proposal itself does not replace either document.

## Acceptance checklist

- [x] The first view contains no decorative space background or marketing copy.
- [x] Run state, active step, and Run action are identifiable without scanning the feed.
- [x] Agent colour appears only on planet discs and small agent markers.
- [x] No uppercase label transformations, synthetic progress bars, or ambient animation remain.
- [x] Every planned, system, and revision step still appears in the roster.
- [x] Live approval remains blocking; replay approval remains read-only.
- [x] Completion still produces the orbital alignment and never hides the result behind it.
- [x] Desktop, narrow, and phone layouts have no horizontal page overflow.
- [x] Keyboard operation, focus restoration, reduced motion, and screen-reader equivalents remain intact.
- [x] The page remains one file with no additional dependencies.
- [x] `cargo test` passes after implementation.

## Review against the brief

A generic minimalist dashboard would use a white background, neutral grey cards, and a large centred task field. That direction was rejected because it discards Sol-O's instrument identity and makes the live run secondary to the input.

The final direction keeps the existing warm graphite palette and three-column run anatomy, but removes the decorative space rendering and duplicated status styling. The memorable element is the working orrery and its final alignment, not a new typography treatment or a collection of cards. The result is quieter without becoming interchangeable with a generic admin template.

## Implementation record

Accepted in `web/index.html` and reconciled into `DESIGN.md`; `PRODUCT.md` changed where the
approval gate and Run plan wording moved. The checklist above was walked in Chromium at
1440x900, 1024x768, and 390x844 (idle, live replay, complete and partial results, result rail,
drawer, read-only replayed gate, seeded live gate, keyboard focus and Tab trap, Escape and
scrim behaviour, and reduced motion). No page-level horizontal overflow was measured at any
of the three widths, and the browser console stayed clean.
