# Design

`register: product`

## Scene

A developer demos a live run on a laptop, screen-sharing in a dim room, while explaining what each planet is doing. The console is the only lit surface. Warm graphite keeps the stage from turning into a blue-black space cliché, and the amber accent reads as instrument light rather than a call-to-action everywhere.

The design is a quiet orrery: a precise run console with a flat orbital diagram at its centre and almost no decorative chrome. The Sun, the planets, and the final alignment carry the product identity. Everything else supports reading and operating the run.

## Colour

Restrained: tinted neutrals, one accent, and one line colour. Planet hues are the only other saturated colour.

| Token | Value | Use |
| --- | --- | --- |
| `--bg` | `#0c0b09` | page, stage, task well |
| `--surface` | `#13110e` | header, rails, dock, result, drawer, gate card, tooltip |
| `--fg` | `#ece7dc` | primary reading text, active controls |
| `--muted` | `#a39a89` | secondary text, timestamps, inactive step numbers, agent labels |
| `--line` | `#2b2720` | every structural divider and orbit ring |
| `--accent` | `#e9b45c` | Sun, Run, Approve and send, focus ring, Replay, current target |
| `--success` | `#8cc79d` | done states and dots |
| `--warning` | `#e0a35c` | waiting for approval, partial, needs clarification |
| `--error` | `#e28372` | failed states, dots, failed feed text |
| `--info` | `#8fb3d9` | planning and called states |

Planet accents, one per agent, with a fallback palette for unknown names:

| Agent | Hue |
| --- | --- |
| researcher | `#43c6ff` |
| writer | `#ff9f43` |
| checker | `#2dd4bf` |
| logger | `#a78bfa` |
| notifier | `#f472b6` |

The only solid accent fills are **Run** and **Approve and send**. The status dot, orbit rings, and agent dots use semantic and agent colour; no row is filled by agent or status. There are no gradients, glass, glow, nebula, noise, or decorative shadow. The one scrim is `rgba(8, 7, 6, 0.72)` behind the live approval gate.

All text pairs clear WCAG AA on both `--bg` and `--surface`: `--fg` 15.3:1, `--muted` 6.8:1, `--accent` 10:1, `--success` 9.7:1, `--warning` 8.6:1, `--error` 6.9:1, `--info` 8.6:1, and Run's `#17130c` on `--accent` 9.8:1.

## Type

Space Grotesk for reading and controls, JetBrains Mono only for timestamps, durations, run IDs, step numbers, and raw output. No display face, no uppercase transforms, no wide tracking.

| Role | Specification |
| --- | --- |
| Body and result | 14px / 1.6, weight 400 |
| Wordmark | 15px / 1.2, weight 600, `Sol-O` as written |
| Stage state | 20px / 1.3, weight 500, sentence case |
| Agent names | 13px / 1.4, weight 600 |
| Primary controls | 13px / 1.4, weight 500–600 |
| Section headings | 12px / 1.4, weight 600, sentence case |
| Metadata, activity, roles | 12px / 1.55, weight 400 |
| Approval explanation | 13px / 1.55, weight 400 |
| Raw output | 12px / 1.55, JetBrains Mono |
| Timestamps, step numbers, plan count, run ID | 11px, JetBrains Mono, tabular numerals |

Times, durations, step counts, and the plan count use `font-variant-numeric: tabular-nums`. Result text keeps a comfortable measure of about 68 characters wherever the surface is wide enough.

## Shape and spacing

- 2px radius on the task input, the approval surface, and the tooltip; buttons and structural regions are square.
- 8px spacing base: 8px between a control and its neighbour, 12px inside rows, 16px around surfaces, 24px between unrelated groups.
- Dividers are 1px `--line` and appear only between unlike regions or adjacent run-plan rows.
- No shadows.

## Layout

Desktop is the existing three-column workspace: left rail `clamp(320px, 28vw, 400px)`, fluid stage, 300px run-plan rail, and a command dock across the bottom.

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ Sol-O              ● Running                            Replay a run…    │
├────────────────────────┬───────────────────────────────┬─────────────────┤
│ Activity               │                               │ Run plan        │
│                        │            orbit rings        │ 2 of 5 complete │
│ 14:02:11  researcher   │              ●  Sol-O         │ 01 Researcher   │
│ called                 │           Running             │    Done         │
│                        │                               │ 02 Writer       │
│                        │       ○       ○       ○       │    Working      │
│                        │                               │ …               │
├────────────────────────┴───────────────────────────────┴─────────────────┤
│ Plan a trip   Review code   Write a report                              │
│ [ Give the Sun a task…                                      ] [ Run ]   │
└──────────────────────────────────────────────────────────────────────────┘
```

- Header: one row, measured into `--header-h` (56px fallback), spanning all columns.
- Left rail: activity feed in the flexible row, result in the row below it, capped at `min(46vh, 520px)`. The result never covers the stage.
- Right rail: **Run plan**, one row per step including system and revision steps. Its count lives in the heading, not the dock.
- Dock: examples on a wrapping row above the task input and Run. Its height is measured into `--dock-h` (76px fallback).
- Below 1180px: one column — header, full-width stage, dock. The rails move into a bottom sheet, inset 16px plus safe-area insets, sitting above the measured dock, with **Run plan** and **Activity** tabs and a collapse toggle. The result and drawer become full-screen reading surfaces.
- Below 600px: at least 16px side gutters (plus safe-area insets), 44px touch targets, no page-level horizontal scroll. The examples wrap above the task input; the replay select takes the second header row.
- Stacking: canvas 0, shell 1, sheet 28, drawer 30 (32 as a sheet), result 34, tooltip 35, approval gate 40.

## Components

### Header

`Sol-O` wordmark, a quiet run-state button, and replay selection. The state button is a 7px semantic dot plus a sentence-case label (`Idle`, `Running`, `Waiting for approval`, `Complete`, …); during replay it reads `Replay — Complete`. It keeps the click-to-reopen-last-result behaviour. Replay pause and speed controls appear only during replay.

### Stage

The stage is the single expressive element, drawn on one full-viewport canvas that is decorative to assistive technology.

- Solid `--bg`; no stars, nebula, noise, camera drift, rays, halos, textures, moons, ring bands, or shading.
- Thin solid `--line` orbit rings, one per planet including system and revision planets.
- Flat filled planet discs in agent colour. The only marks are a small × for a failed step, an `attempt n` label in muted mono after a retry, and the agent name below the disc.
- The Sun is an 18px amber disc with `Sol-O` beneath it, then the run state at 20px/500 in sentence case, then the latest detail. Idle reads `Ready for a task`.
- Orbit placement fans planets across the upper arc so the Sun's caption stays clear.
- The final alignment is the only orchestrated animation: planets travel onto the Sun's row over 1200ms, hold for 3000ms, then release over 1200ms. Planet labels are suppressed while the line-up holds so they cannot cover the caption.
- Canvas tooltips remain for pointer users; the Run plan is the keyboard and screen-reader equivalent. The canvas pauses when the tab is hidden and redraws on demand when nothing is animating.

### Run plan

The visible heading is **Run plan** with the count (`2 of 5 complete`) in muted mono. One row per planned, system, or revision step, in step order; a revision can show the same agent twice under different row numbers.

Each row shows the row's sequence number in muted mono, a small agent-colour dot, the agent name, a short role or task summary, and a sentence-case state from `Waiting`, `Called`, `Working`, `Done`, or `Failed`. The whole row is the detail button (`role="button"`, `tabindex="0"`, Enter and Space, visible focus). There are no progress bars, shimmer, chips, or row tints.

Empty copy: `Start a run to see its plan.` Before the plan arrives: `Waiting for the Sun's plan…`.

### Activity

A quiet log: 11px mono timestamp, a 6px status or agent dot, and a plain-language summary at 12px. Summaries wrap; failures and tool details are never clipped. Failed lines take the error colour and done lines the success colour on the text and dot only — no row fills.

The feed keeps its 40-line cap, newest at the bottom, append-only live region, and `Hide`/`Show` toggle. Empty copy: `Activity appears when a run starts.`

### Result

The left rail's lower row on desktop; a full-screen sheet below 1180px. A plain `Result` header, a 2×2 metadata group of separate labelled values (State, Duration, Verdict, Run), the body at 14px within 68 characters, and one quiet Replay action. It keeps `role="dialog"`, takes focus on open, restores focus to its opener, and opens automatically on terminal events (after the alignment on small screens). It does not animate in.

### Step details

The drawer stays a fixed overlay on the right rail on desktop (300px wide), and a full-screen surface below 1180px. It shows the agent and step, a plain status word, Role, Task, Duration, and **Raw output**, using the same type and spacing as the result. No tabs, accordions, or nested cards. Escape closes it, focus returns to the row that opened it, and it hides the result while open.

### Approval gate

The live gate is centred over a `rgba(8, 7, 6, 0.72)` scrim, `aria-modal`, and blocking. One plain surface with a 20px/500 heading `Approval required`, the explanation `Nothing has been sent. Approving sends the stored message once. Rejecting ends this action.`, the stored message preview (wrapping and scrolling when long), and the controls `Reject` and `Approve and send`. Approve and send is the only other solid amber fill after Run; Reject is quiet. Tab stays on the two controls; Escape and a scrim click never decide or dismiss.

A replayed gate sits below the header with no scrim, no `aria-modal`, no buttons, and the read-only copy `Nothing has been sent. This stored gate is read-only and offers no decision.` Approval decisions never share a line with run state.

### Command dock

Three example tasks as quiet text buttons on a wrapping row, the task well, and Run. Enter in the task starts a run; Run is the only solid accent control in the dock and there is no plan counter.

## Motion

- Text, dots, rows, and result visibility change immediately.
- Control feedback is at most 120ms on colour and border; there is no hover movement or press translation.
- Only planet travel and the completion alignment animate; no ambient motion runs while idle.
- `prefers-reduced-motion: reduce` snap all state and alignment changes and removes the frame loop; it never delays access to the result or approval controls.

## Bans

- Glassmorphism, `backdrop-filter`, floating cards, shadows, gradients.
- Uppercase labels, wide tracking, synthetic progress bars, shimmer, glow, halos.
- Side-stripe borders, hero-metric templates, nested cards, identical card grids.
- Ambient canvas motion (star twinkle, parallax, corona pulse, ray rotation, orbit drift).
- Raw run-state identifiers in the label, the Sun, or the result meta; a run state concatenated with an approval decision.
- A marketing hero inside the console.
