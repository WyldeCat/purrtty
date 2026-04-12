# Block Mode (v0.3)

## Overview

Block mode renders each agent interaction as a visually distinct
**block** — a bordered region in the terminal grid that groups the
user's prompt, the agent's streamed output, tool invocations, and a
status footer into one cohesive unit. This matches the UX of Warp's
"blocks" and distinguishes agent activity from normal shell output.

Normal shell I/O still flows through the grid as raw text. Blocks
only appear when the user triggers the agent via `>`.

## Visual anatomy of a block

```
┌─ Agent ─────────────────────────────────────┐
│ > what does this function do?               │  ← user prompt (dim cyan)
│─────────────────────────────────────────────│
│ This function parses VT escape sequences    │  ← agent text output
│ and dispatches them to the grid model.      │
│                                             │
│ ⚡ Read src/parser.rs                       │  ← tool invocation
│   fn csi_dispatch(...) {                    │  ← tool output (dim)
│     ...                                     │
│                                             │
│ The main entry point is `advance()` which   │  ← more agent text
│ feeds bytes through a vte state machine.    │
├─────────────────────────────────────────────┤
│ ✓ Done — 4.6s                              │  ← status footer
└─────────────────────────────────────────────┘
```

While the agent is running, the footer shows a live status:

```
├─────────────────────────────────────────────┤
│ ⠹ Thinking... — 12s                        │  ← thinking
├─────────────────────────────────────────────┤
│ ⚡ Running: Bash — 3s                       │  ← tool executing
├─────────────────────────────────────────────┤
│ ⠸ Streaming... — 8s                         │  ← text streaming
└─────────────────────────────────────────────┘
```

## Design decisions

### 1. Blocks are virtual overlays, not grid content

Blocks are NOT written to the terminal grid as raw ANSI. Instead,
the renderer draws them as an overlay on top of the grid content.
The grid itself still receives the raw text/tool output via
`echo_to_terminal`, but the block chrome (borders, header, footer,
background) is drawn by the renderer.

**Why:** Writing ANSI border characters to the grid would:
- Pollute scrollback with decoration
- Break on resize (borders at fixed columns)
- Interfere with reflow
- Make copy/paste grab border chars

### 2. Block state machine

Each block has a lifecycle:

```
                          ┌──────────┐
         user types >     │  Input   │  user composing prompt
                          └────┬─────┘
                               │ Enter
                          ┌────▼─────┐
                          │ Thinking │  waiting for first output
                          └────┬─────┘
                               │ text_delta / tool_use
                    ┌──────────┼──────────┐
                    │          │          │
               ┌────▼────┐ ┌──▼───┐ ┌───▼────┐
               │Streaming│ │ Tool │ │  Tool  │
               │  text   │ │ Use  │ │ Output │
               └────┬────┘ └──┬───┘ └───┬────┘
                    │         │         │
                    └─────────┼─────────┘
                              │ (cycles between these)
                         ┌────▼─────┐
                         │   Done   │  agent finished
                         └──────────┘
```

### 3. Data model

```rust
struct Block {
    /// The user's prompt text (what they typed after `>`).
    prompt: String,
    /// Segments of output, in order.
    segments: Vec<BlockSegment>,
    /// Current lifecycle state.
    state: BlockState,
    /// When the agent was spawned (for elapsed time).
    started_at: Instant,
    /// Grid row where the block starts (for rendering overlay).
    start_row: usize,
}

enum BlockSegment {
    /// Agent text output (may contain ANSI color codes).
    Text(String),
    /// Tool invocation header + streamed input/output.
    Tool {
        name: String,
        input: String,
        output: Option<String>,
    },
}

enum BlockState {
    /// Agent spawned, no output yet.
    Thinking,
    /// Receiving text_delta events.
    Streaming,
    /// A tool_use block is active.
    ToolRunning { name: String },
    /// Agent process exited.
    Done { exit_code: i32, elapsed: Duration },
}
```

### 4. Rendering

The renderer receives a `BlockOverlay` struct alongside the grid:

```rust
struct BlockOverlay {
    /// Grid row where the block starts (absolute row in scrollback + visible).
    start_row: usize,
    /// Number of grid rows the block occupies.
    row_count: usize,
    /// The user's prompt text (displayed in the header).
    prompt: String,
    /// Status text for the footer (spinner + tool + elapsed).
    footer: String,
    /// Block lifecycle phase — controls border color and footer.
    state: BlockOverlayState,
}

enum BlockOverlayState {
    /// Active: blue border, live footer with spinner.
    Active,
    /// Completed successfully: dim gray border, "✓ Done" footer.
    Done,
    /// Completed with error: red border + red tint, "✗ Failed" footer.
    Error,
}
```

The renderer draws:
1. **Full border**: a 1-2px rounded-rect border around the entire
   block (accent blue while active, dim gray when completed, red on
   error). Corners are approximated with small quads — no actual
   round rasterization needed.
2. **Background tint**: a subtle darker/lighter wash over block rows
   to set them apart from shell output.
3. **Header**: the user's prompt (`> what does this function do?`)
   pinned at the top of the block, separated from the output by a
   thin horizontal rule.
4. **Sticky header**: when the user scrolls and the block's top edge
   moves above the visible area, the header sticks to the top of the
   viewport (like Warp's "Sticky Command Header"). Clicking it
   scrolls back to the block start.
5. **Footer**: the status line at the bottom of the block, inside
   the border. Shows spinner + state + elapsed time while active,
   `✓ Done — 4.6s` when finished.
6. **Error state**: exit_code != 0 → border turns red, background
   gets a subtle red tint (matching Warp's red sidebar + red bg).

No box-drawing characters in the grid — all chrome is quad overlays.

### 5. Interaction with existing systems

**Grid:** Raw agent text still goes to the grid via echo_to_terminal.
The block tracks which grid rows it occupies by remembering the
cursor row at block start and watching cursor movement.

**Scrollback:** When block rows scroll into scrollback, the block's
`start_row` is adjusted. The overlay still renders for scrollback
rows that are visible (honoring scroll_offset).

**Reflow:** Block row tracking uses absolute row indices (same as
selection). On resize/reflow, the block's start_row and row_count
are recalculated by walking the grid and finding the block's first
content row.

**Selection:** Selection works normally — the user selects the raw
text in the grid, not the border chrome. Copy grabs clean text.

**Tabs:** Each session owns its own block (if any). Only the active
session's block overlay is passed to the renderer.

### 6. Spinner animation

The footer spinner uses braille dots cycling at ~100ms:
`⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`

A `StatusTick` UserEvent fires every 100ms while a block is active,
triggering a redraw that advances the spinner. The timer thread is
spawned when the block enters `Thinking` and stopped on `Done`.

### 7. Elapsed time

Displayed in the footer: `— 12s` or `— 1m 23s`. Computed from
`block.started_at` on each render (no separate timer needed — the
spinner timer drives the redraw and the elapsed is read from
`Instant::now() - started_at`).

## Implementation plan

### Step 1 — Block data model
- [ ] Add `Block`, `BlockSegment`, `BlockState` to a new
      `src/block.rs` module in purrtty-app.
- [ ] Block::new(prompt, start_row, started_at).
- [ ] Methods: push_text, start_tool, finish_tool, set_done.

### Step 2 — Wire agent events into Block
- [ ] When user submits agent prompt (Enter in AgentInput), create a
      Block and store it on the Session.
- [ ] In the agent reader callback (on_output), parse stream-json
      events and call block.push_text / block.start_tool etc.
      instead of (or in addition to) echo_to_terminal.
- [ ] On AgentFinished, call block.set_done.

### Step 3 — Spinner timer
- [ ] Add `UserEvent::StatusTick { session_id }`.
- [ ] Spawn a 100ms timer thread when block enters Thinking.
- [ ] On StatusTick, request a redraw (the renderer reads the block's
      state and renders the updated spinner + elapsed).
- [ ] Stop the timer on Done.

### Step 4 — Renderer overlay
- [ ] Add `BlockOverlay` to the render parameters.
- [ ] Draw left accent bar for block rows.
- [ ] Draw subtle background tint.
- [ ] Draw footer text (spinner + status + elapsed) at the block's
      last row + 1.

### Step 5 — Scroll + reflow integration
- [ ] Track block start_row in absolute coordinates.
- [ ] On reflow (resize), recalculate start_row.
- [ ] Renderer converts absolute block rows to view rows honoring
      scroll_offset (same math as selection overlay).

### Step 6 — Tests
- [ ] Block state machine transitions.
- [ ] BlockSegment push ordering.
- [ ] Footer text formatting at various elapsed times.
- [ ] Overlay row calculation with scroll offset.

## Out of scope (this pass)

- Collapsible blocks (click to fold/unfold).
- Block-level copy (select entire block).
- Drag to reorder blocks.
- Persistent block history across sessions.
- Multiple concurrent blocks (one per agent run is enough).
