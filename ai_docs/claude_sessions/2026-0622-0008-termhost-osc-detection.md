# herdr (Rust) — Phase B: termhost pane runtime, OSC + detection consumption

**Date:** 2026-0622-0008
**Session ID:** `231ee2d1-0169-4dd7-b77b-7e2bbdf885c2`
**Repo:** `~/projs/rust/herdr` (Rust orchestrator) · paired with `~/projs/go/herdr-web` (Go terminal backend)
**Branch:** `roh/phase-b-termhost-client`
**Remotes:** `origin` = `rohanthewiz/herdr-go` (**pushable, canonical**); `herdr-origin` = `ogulcancelik/herdr` (upstream, **no write access** — do not push)

> Rust-side record. The Go-side companion lives in the herdr-web repo at
> `ai_docs/claude_sessions/2026-0622-0008-termhost-osc-detection.md`.
> The seam contract is documented (in herdr-web) at `ai_docs/phase-b-orchestration-seam.md`.

---

## Strategic context

**Diverging from upstream herdr.** `rohanthewiz/herdr-go` (this Rust repo's `origin`)
is now canonical. Endgame: **Go becomes the single terminal backend** (PTY + VT +
detection); the Rust in-process PTY/ghostty/detect path is transitional and will be
retired for termhost panes. All termhost code is behind `--features termhost` and the
`HERDR_TERMHOST_SOCKET` env var; default builds are byte-for-byte unaffected.

## What shipped (Rust side), in order

### 1. Route the live pane runtime through termhost (step 3) — `038d45a`
- `PaneRuntimeIo::Termhost(Arc<TermhostPane>)` variant; input/resize/shutdown route to
  the Go backend; handoff/fd ops are inert.
- `spawn_command_builder` branches to `finish_termhost` when `crate::termhost::client_if_enabled()`
  (env-gated, `OnceLock`-cached process-wide client). No public spawn-signature churn.
- `render()` / `collect_dirty_patch()` / `cursor_state()` branch on the backend — both the
  retained fast path and the full-render fallback draw Go-emulated cells (call sites unchanged).
- The client accumulates Go's full+`skip`-diff frames into a retained grid.
- Exit watcher task polls `exit_status()` → `AppEvent::PaneDied`.
- `protocol/wire.rs`: `u32_to_color`/`u16_to_modifier` made `pub(crate)` for the CellData→ratatui path.
- The local `PaneTerminal` is kept but **unfed** (emulator-derived queries return empty) —
  this is the slice-1 degraded behavior; metadata comes from Go signals (below).

### 2. e2e smoke test — `9cd8f35`
- `tests/termhost_e2e.rs` (gated `#![cfg(feature="termhost")]`, skips without a daemon):
  spawns a real `herdr server` with `HERDR_TERMHOST_SOCKET`, `workspace.create` → pane on
  the Go daemon, drives input, asserts the marker renders in a client `SemanticFrame` AND
  that `pane.read` (local emulator) is empty — proving Go-backed, not in-process fallback.
- **Gotchas captured:** headless server needs `workspace.create` to spawn a pane (no auto-spawn);
  `pane.*` text APIs read the local (unfed) emulator → assert via the rendered client frame.

### 3. OSC 7 cwd passthrough (Rust side) — `3ef1b0d`
- `proto.rs`: `Event::PaneCwd`.
- `client.rs`: a per-pane sink (introduced as `OscSink`/`PaneOsc`, generalized in step 4).
- `pane.rs` `finish_termhost`: sink closure calls the existing `publish_reported_cwd` (updates
  `reported_cwd` + emits `TerminalCwdReported`), so `PaneRuntime::cwd()` (new-pane inheritance,
  worktree) works for termhost panes. e2e: drives OSC 7, asserts `pane.get` cwd == `/tmp`.

### 4. Consume Go-side agent detection — Stage A (identity) — `51f77f0`
- **Detection-in-Go decision** (after studying `src/detect`): Go owns detection (it owns the
  PTY child); Rust consumes results. Detection has 3 layers — pure manifest engine, process
  probing, driver state machine — all ported to Go in stages. **Key reuse:** the Go detector
  only needs to feed the existing `StateChanged` path; all downstream (borders, sidebar, status
  dots, notifications, `agent_status`, "seen" flag, hook-vs-screen arbitration) is untouched.
- `proto.rs`: `Event::PaneAgent { agent, state, visible_blocker, visible_working }`.
- `client.rs`: generalized the per-pane sink to `PaneSignal` (`Cwd` + `Agent`) / `SignalSink`.
- `pane.rs` `finish_termhost`: `PaneSignal::Agent` → `AppEvent::StateChanged`
  (`parse_agent_label(agent)` + state string → `AgentState`), the exact path the Rust screen
  detector fed. **The Rust screen-scan task is no longer spawned for termhost panes**
  (it only read the unfed local emulator); `detect_handle`/notify/pending are inert placeholders.
- e2e `termhost_pane_reports_agent_identity`: `exec -a claude sleep` → `pane.get` agent == "claude".

### 5. Stage B (manifest state) — Rust side e2e only — `a24a6aa`
- Go now classifies state via the ported manifest engine. **No Rust library change needed** —
  Stage A's mapping already carries `state` + visible flags through.
- e2e `termhost_pane_reports_agent_working_state`: a pane running `exec -a pi sh -c 'printf
  Working...'` reaches `pane.get` with `agent=pi`, `agent_status=working`.

### 6. Consume Go-side OSC 52 clipboard — `5ce148a`
- Go reconstructs OSC 52 clipboard writes from the raw PTY (libghostty-vt drops them) and emits a
  new `pane_clipboard` seam event. Rust re-emits it through herdr's own clipboard writer — same
  `AppEvent::ClipboardWrite { content }` the in-process PTY path uses (the integration philosophy:
  map Go signals onto *existing* AppEvents).
- `proto.rs`: `Event::PaneClipboard { pane_id, data: Vec<u8> }` + a `b64_deserialize` (mirror of
  `b64_serialize`; matches Go's `[]byte`→base64 JSON). Empty `data` = clipboard-clear.
- `client.rs`: `PaneSignal::Clipboard(Vec<u8>)` + dispatch arm to the per-pane sink.
- `pane.rs` `finish_termhost`: `PaneSignal::Clipboard(content)` → `AppEvent::ClipboardWrite`.
- Tests: proto `pane_clipboard_decodes_base64` + `pane_clipboard_empty_is_clear`. (No new e2e —
  asserting a real system-clipboard write from a child is environment-dependent; the Go side has
  the `TestHostReportsPaneClipboard` integration test that the event is emitted.)

### 7. Show termhost OSC 0/2 window title on the pane border — `7f32edf` (Go emit: herdr-web `6807bbb`)
- Go raw-scans OSC 0/2 (libghostty surfaces it to its emulator for detection, but the seam carried
  none) and emits a new `pane_title` event. Rust consumes it as **terminal chrome**.
- **Finding:** herdr did NOT previously show the raw OSC title as chrome — `border_label` is
  hook-metadata title (`effective_title`) → manual label → agent label; the raw OSC 0/2 title was
  only detection evidence. So this is **new** border behavior (termhost panes only, since only they
  populate `terminal_title`).
- `proto.rs`: `Event::PaneTitle { pane_id, title }` (+ `pane_title_decodes` test).
- `client.rs`: `PaneSignal::Title(String)` + dispatch arm.
- `pane.rs` `finish_termhost`: `PaneSignal::Title` → `AppEvent::TerminalTitleReported { pane_id,
  title: non-empty-or-None }`.
- `events.rs`: `AppEvent::TerminalTitleReported`. `app/actions.rs`: handler resolves pane→terminal
  and `set_terminal_title` (chrome only — **not** session-persisted, no dirty mark; mirrors the
  `TerminalCwdReported` arm).
- `terminal/state.rs`: new `terminal_title` field + `set_terminal_title`; **`border_label`
  precedence: hook title > OSC title > manual label > agent label** (user-chosen via the option
  preview — note the OSC title shadows a manual label; one-line swap if undesired). `TerminalState`
  isn't `Serialize` (rebuilt via `new()` on restore), so the new field is persistence-safe.
- Tests: `border_label_uses_terminal_title_above_agent_below_manual` + proto decode.

### 8. Carry termhost OSC 8 hyperlinks into the frame render path — `5a4aa23` (Go emit: herdr-web `96bec8b`)
- Unlike the other passthroughs, OSC 8 is *inline per-cell* (a link wraps grid cells), so it rides
  the **frame/grid path**, not a `pane_*` event. Go raw-extracts URIs (libghostty exposes them only
  via `GridRef.HyperlinkURI`) and sends a per-cell `hyperlink` index + a frame `hyperlinks` URI table.
- **Almost no Rust change needed:** `render_ansi` **already** emits OSC 8 from `FrameData`
  (`cell.hyperlink` → `hyperlinks[index]`), and `proto::Frame` already deserializes cells straight
  into `wire::CellData` (which has `hyperlink`). The only gap was the URI table.
- `proto.rs`: `Frame.hyperlinks` (`#[serde(default)]`) + `into_frame_data` carries it.
- `client.rs`: `PaneGrid.hyperlinks` folded through `apply`/`snapshot`. Link-bearing frames are sent
  **full** by Go, so the table and cell indices always replace together — no stale-index risk.
- Tests: proto `frame_with_hyperlinks_carries_table_and_indices`. (Go side has the real-OSC-8
  `TestHostReportsHyperlinkFrame` integration test.)
- **Limitation:** lifts hyperlinks for the **frame-data render path** (web/remote ANSI stream). The
  native-TUI mouse resolver `visible_hyperlinks` still reads the *unfed* local emulator for termhost
  panes (`viewport_hyperlink_uri`), so TUI click-to-open won't see them — separate follow-up.

### 9. Drive termhost scrollback through the Go backend — `048875f` (Go emit: herdr-web `3fc51c5`)
- Needs a **command** (Rust→Go), not just an event. The Go backend now has real scrollback
  (libghostty defaulted to 0 history!), a `ScrollViewport` command, and reports its position on each
  frame. Previously `PaneRuntime`'s scroll path fell through to the *unfed placeholder* emulator.
- `proto.rs`: `Command::ScrollViewport { pane_id, delta }` (neg=up, pos=down; Go clamps so a big
  positive delta = scroll-to-bottom) + `Frame.scroll` (`FrameScroll`, `#[serde(default)]`).
- `client.rs`: `PaneGrid.scroll` retained through `apply` (frames omit it when no history);
  `TermhostPane.scroll(delta)` / `scroll_metrics()`.
- `pane.rs`: `PaneRuntime`'s `scroll_up/down/reset/set_scroll_offset_from_bottom/scroll_metrics`
  branch to the termhost backend, mapping to/from the seam delta (`set_offset` computes the delta
  from the last reported offset; `scroll_reset` sends `i32::MAX`). No `TerminalBackend` trait change
  — `PaneRuntime` reaches the pane via the concrete `io.termhost_pane()` handle.
- Tests: proto scroll-command serialize + frame-scroll decode (+ default-none). (Go side has the
  empirical `TestScrollback` + host `TestHostScrollbackReportsMetrics`.)
- **Follow-up:** new output snaps the viewport to the bottom (no scroll-lock/pinning yet).

---

## Key facts for future me

- **The whole termhost surface is `#[cfg(feature="termhost")]` + env-gated.** Default build
  unaffected (1891 tests pass).
- **Integration philosophy:** map Go signals onto *existing* AppEvents/paths, don't invent new
  consumers. cwd → `TerminalCwdReported` (via `publish_reported_cwd`); agent → `StateChanged`.
- **`AgentStatus` serializes snake_case** (`working`/`idle`/`blocked`/`done`/`unknown`); derived
  from `(AgentState, seen)` in `pane_agent_status` (`src/app/api_helpers.rs`).
- **Degraded for termhost panes (by design, until more Go signals land):** selection, kitty
  graphics. (scrollback ✅ item 9.) (OSC title-as-chrome ✅ item 7; OSC 8 hyperlinks ✅ item 8 for the web render
  path — TUI click resolution still pending.) Key encoding stays Rust-side (raw bytes).
- **Build/run:** `export ZIG="~/projs/go/herdr-web/.tools/zig-wrapped"`; run e2e with
  `HERDR_TERMHOST_SOCKET=<sock> cargo test --features termhost --test termhost_e2e -- --test-threads=1`
  against a running Go `cmd/termhost` daemon.
- **Push target:** `git push origin roh/phase-b-termhost-client` (NOT `herdr-origin` — 403).

## Commits on `roh/phase-b-termhost-client` (this session)

```
048875f feat: drive termhost scrollback through the Go backend
5a4aa23 feat: carry termhost OSC 8 hyperlinks into the frame render path
7f32edf feat: show termhost OSC 0/2 window title on the pane border
5ce148a feat: consume Go-side OSC 52 clipboard for termhost panes
a24a6aa test: e2e for manifest-driven agent working state (Stage B)
51f77f0 feat: consume Go-side agent detection for termhost panes (Stage A)
3ef1b0d feat: route OSC 7 cwd from the termhost backend (Rust side)
9cd8f35 test: e2e smoke test for the termhost backend (step 3)
038d45a feat: route the live pane runtime through termhost (step 3)
14b2212 feat: termhost terminal-backend seam (feature-gated)  [prior session]
1e4dd9a feat: Rust client for the Go↔Rust orchestration seam  [prior session]
```

## Next steps

- **Stage C — driver parity (Go side):** ✅ shipped Go-side (debounce + process-probe throttle).
  OSC 9 progress now also fed into Go-side detection. No Rust change needed (it applies `StateChanged`).
- **OSC 52 clipboard** ✅ done (item 6) — Go emits `pane_clipboard`, Rust → `AppEvent::ClipboardWrite`.
- **OSC 0/2 title** ✅ done (item 7) — Go emits `pane_title`, Rust → `terminal_title` → border chrome.
- **Selection (next):** decided model — Go request/response. `RequestSelection { pane_id, anchor,
  cursor }` command → ghostty selection formatter (`WithSelection`) → `pane_selection { text }`
  event → `AppEvent::ClipboardWrite`. The item-9 scroll metrics are the foundation for its absolute
  (screen-buffer) coordinates. Wire `PaneRuntime.extract_selection` to the termhost backend.
- **kitty graphics** passthrough — the last degradation.
- **Follow-ups:** scroll-lock/pinning (output snaps to bottom today); TUI hyperlink click resolver
  (`visible_hyperlinks` still reads the unfed local emulator).
- Eventually: flip termhost to default, then **delete `src/pty` / `src/ghostty` / `src/terminal`**
  for the pane path and drop the unfed-emulator placeholder in `finish_termhost`.
