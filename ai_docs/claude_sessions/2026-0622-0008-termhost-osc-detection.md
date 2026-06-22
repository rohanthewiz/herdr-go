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

---

## Key facts for future me

- **The whole termhost surface is `#[cfg(feature="termhost")]` + env-gated.** Default build
  unaffected (1891 tests pass).
- **Integration philosophy:** map Go signals onto *existing* AppEvents/paths, don't invent new
  consumers. cwd → `TerminalCwdReported` (via `publish_reported_cwd`); agent → `StateChanged`.
- **`AgentStatus` serializes snake_case** (`working`/`idle`/`blocked`/`done`/`unknown`); derived
  from `(AgentState, seen)` in `pane_agent_status` (`src/app/api_helpers.rs`).
- **Degraded for termhost panes (by design, until more Go signals land):** selection, scrollback,
  hyperlinks, kitty graphics, OSC title-as-chrome. Key encoding stays Rust-side (raw bytes).
- **Build/run:** `export ZIG="~/projs/go/herdr-web/.tools/zig-wrapped"`; run e2e with
  `HERDR_TERMHOST_SOCKET=<sock> cargo test --features termhost --test termhost_e2e -- --test-threads=1`
  against a running Go `cmd/termhost` daemon.
- **Push target:** `git push origin roh/phase-b-termhost-client` (NOT `herdr-origin` — 403).

## Commits on `roh/phase-b-termhost-client` (this session)

```
a24a6aa test: e2e for manifest-driven agent working state (Stage B)
51f77f0 feat: consume Go-side agent detection for termhost panes (Stage A)
3ef1b0d feat: route OSC 7 cwd from the termhost backend (Rust side)
9cd8f35 test: e2e smoke test for the termhost backend (step 3)
038d45a feat: route the live pane runtime through termhost (step 3)
14b2212 feat: termhost terminal-backend seam (feature-gated)  [prior session]
1e4dd9a feat: Rust client for the Go↔Rust orchestration seam  [prior session]
```

## Next steps

- **Stage C — driver parity (Go side):** pending-idle debounce, startup grace, content-skip,
  re-check cadences — to kill flicker. No Rust change expected (it just applies `StateChanged`).
- **OSC 52 clipboard** Go→Rust → `AppEvent::ClipboardWrite` (extend the `PaneSignal` sink).
- **OSC title/scrollback/selection/hyperlinks/kitty** passthrough to lift the remaining
  termhost degradations.
- Eventually: flip termhost to default, then **delete `src/pty` / `src/ghostty` / `src/terminal`**
  for the pane path and drop the unfed-emulator placeholder in `finish_termhost`.
