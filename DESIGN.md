# Design

`register: product`

## Scene

A developer demos a live run on a laptop, screen-sharing in a dim room, while explaining what each planet is doing. The console is the only lit surface. Warm graphite keeps the stage from turning into a blue-black space cliché, and the amber accent reads as instrument light rather than a call-to-action everywhere.

## Colour

Restrained: tinted neutrals plus one accent. Planet hues are the only other saturated colour.

| Token | Value | Use |
| --- | --- | --- |
| `--bg` | `#0c0b09` | page, input and select wells, pill fill |
| `--deep` | `#080706` | darkest neutral; declared but not referenced by any surface today |
| `--surface` | `#13110e` | header, dock, both rails, result, drawer, gate card, tooltip |
| `--fg` | `#ece7dc` | primary text |
| `--muted` | `#a39a89` | labels, secondary text |
| `--border` | `rgba(236, 231, 220, 0.13)` | hairlines |
| `--accent` | `#e9b45c` | Run, focus ring, roster header, pressed replay/sheet controls |
| `--success` | `#8cc79d` | done states, Approve fill |
| `--warning` | `#e0a35c` | waiting for approval, partial, needs clarification |
| `--error` | `#e28372` | failed states, Reject |
| `--info` | `#8fb3d9` | planning, called |

Planet accents, one per agent, with a fallback palette for unknown names:

| Agent | Hue |
| --- | --- |
| researcher | `#43c6ff` |
| writer | `#ff9f43` |
| checker | `#2dd4bf` |
| logger | `#a78bfa` |
| notifier | `#f472b6` |

Row tints use the status hues at low alpha (failed row `rgba(226, 131, 114, 0.1)`, done row `rgba(140, 199, 157, 0.07)`). No `#000` or `#fff` fills, no glass, no gradient text, no side-stripe accents. Status chips pair a hue with a matching 45–55% alpha border. `--ok` and `--bad` alias success and error and are declared but unreferenced.

## Type

Space Grotesk for UI, JetBrains Mono for telemetry and labels. Seven sizes, four weights (400, 500, 600, 700).

| Role | Size | Weight | Notes |
| --- | --- | --- | --- |
| Base / body | 14px | 400 | line-height 1.5, Space Grotesk |
| Task input | 14px | 400 | dock well, 38px tall |
| Wordmark | 14px | 700 | 0.24em tracking, uppercase |
| Result draft | 14px | 400 | line-height 1.6, `pre-wrap`, scrolls in the rail |
| Roster agent | 13px | 600 | one line, ellipsis |
| Approval preview | 13px | 400 | line-height 1.55, capped height with scroll |
| Run button | 13px | 700 | 0.08em tracking, uppercase, `#17130c` on accent |
| Roster role | 12px | 400 | muted, ellipsis at 46 characters |
| Buttons, preset chips | 12px | 500–600 | 0.04em tracking |
| Replay select | 12px | 400 | header control |
| Drawer body | 12px | 400 | line-height 1.55 |
| Tooltip body | 12px | 400 | 12.5px for the planet name |
| Caption / eyebrow | 10px | 600–700 | 0.18em tracking, uppercase, muted |
| Status chip | 10px | 700 | 0.1–0.12em tracking, uppercase |
| Status pill | 11px | 700 | 0.12em tracking, uppercase |
| Feed line | 11.5px | 400 | mono, line-height 1.6 |
| Step number, plan count, result meta | 11.5px | 400–600 | mono, tabular numbers |

The result body wraps inside the rail (320–400px), so its measure follows the rail width; no separate character cap is applied. Dense telemetry stays in mono at 10–11.5px; reading copy is 13–14px.

## Layout

- Desktop shell: one grid at `100vh`, columns `clamp(320px, 28vw, 400px) minmax(0, 1fr) 300px`.
- Rows: `56px` header, flexible workspace, `auto` result, `auto` dock. The result row collapses when no result is open and is capped at `min(46vh, 520px)`.
- Areas: header spans all columns; the feed is the left rail's flexible row and the result its lower row; the stage is the centre; the roster spans both workspace rows at 300px; the dock spans the width.
- The canvas is fixed, full-viewport, and decorative. The stage element is measured with `getBoundingClientRect` (and a `ResizeObserver`); the Sun centres on the stage rectangle, and orbit radius derives from `min(stageWidth, stageHeight × 1.7)`.
- Command dock: presets, task, plan count, Run, in that order. Its height is measured into `--dock-h` (76px fallback) so the sheet and drawer can stack above it.
- Below `1180px`: one column — header, full-width stage, dock. The header wraps to two rows and the caption is hidden; the replay select takes the remaining width, and `--header-h` becomes 83px for the surfaces that offset from it.
- Below `1180px` the rails move into a bottom sheet inset 8px, `max-height: 50vh`, sitting `8px + var(--dock-h)` above the bottom, with Roster and Feed tabs and a collapse toggle.
- Below `1180px` the result and the drawer become full-screen sheets with safe-area padding; while one is open the header, dock, and sheet are hidden.
- Stacking order: canvas 0, shell 1, sheet 28, drawer 32, result 34, tooltip 35, approval gate 40.

## Components

- **Header bar**: matte surface, one bottom hairline, no shadow. Wordmark, caption, run-state pill, replay controls, replay select. The pill is a button that reopens the last result when a run id and a result exist.
- **Status pill**: one label from the shared run-state vocabulary (`idle`, `planning`, `running`, `waiting for approval`, `complete`, `partial`, `needs clarification`, `rejected`, `failed`), uppercased, with a `REPLAY ·` prefix during playback. Tone classes colour it; the fill stays matte.
- **Rails**: opaque surfaces, hairline dividers, square edges, no nesting. The left rail stacks the feed over the result; the right rail holds planet rows. Neither has a card treatment.
- **Feed line**: time, dot, agent, event text, all mono. Failed and done lines tint the row background and shift the text colour; the dot carries the agent accent and the agent name uses it. No side stripe. Capped at 40 lines, trimmed from the front, newest at the bottom, appended rather than re-rendered. Empty copy: "No events yet — lines appear here when a run starts."
- **Planet row**: zero-padded step number in the agent accent, agent name, status chip (`Idle`, `Called`, `Working`, `Done`, `Failed`), role, and a 3px progress bar (0/25/60/100/100%). The row itself is the button (`role="button"`, `tabindex="0"`, Enter and Space). Header reads `completed / total steps` once a plan exists; the empty copy names the Sun's plan.
- **Result**: the left rail's lower row — header, mono meta line (state, duration, verdict, short run id), scrolling body, close, Replay. It keeps `role="dialog"`, takes focus on open, restores focus to its opener on close, and opens automatically on terminal events.
- **Approval gate**: centred card (`min(520px, 92vw)`) over a `rgba(8, 7, 6, 0.72)` scrim, one soft shadow, solid success Approve, quiet Reject with a hairline error border. Tab is trapped on the two buttons. A replayed gate sits below the header with no scrim, no buttons, and a read-only label.
- **Drawer**: fixed overlay on the right rail (300px wide, header to dock) on desktop; full-screen sheet below 1180px. Shows role, task, duration, and raw output. It hides the result while open and restores it on close, so the two detail surfaces never collide.
- **Command dock**: preset chips, task well, plan count (`N planets planned`), solid amber Run. Enter in the task starts a run.

## Motion

- Curve: `cubic-bezier(0.16, 1, 0.3, 1)` for chrome transitions; fast `120ms`, slow `240ms`. Canvas easing is in-out.
- Chrome: hover border and background at 120ms; button press is a 1px translate; the roster progress bar animates width at 240ms, and a working row's bar runs a 1.2s background-position shimmer.
- Result: enters over 240ms with an 8px rise and a fade.
- Canvas: corona pulse, ray rotation, planet travel, and the alignment — planets travel onto the Sun's row over 1200ms, hold for 3000ms, then release over 1200ms.
- Reduced motion suppresses the result entrance, the shimmer and bar transition, the alignment travel (it snaps into line), and the decorative pulses; state changes still render.

## Bans

- Glassmorphism, `backdrop-filter`, floating cards.
- Gradient text, side-stripe borders, hero-metric templates.
- Nested cards, identical card grids, modals as the first answer.
- Raw run-state identifiers in the pill, the Sun, or the result meta; a run state concatenated with an approval decision.
- A marketing hero inside the console.
