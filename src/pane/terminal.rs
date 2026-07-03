//! A pane's local terminal-side state.
//!
//! Since WS0 stage C the VT emulation lives exclusively in the Go termhost
//! daemon; the Rust side keeps only mirrored input modes + pure encoders
//! ([`InputMirror`], WS0 stage B2) and the shared plain-data types the app
//! layers consume. Buffer-reading methods on the `Mirror` variant return
//! empty defaults — `PaneRuntime`'s Go-backend arms answer those queries
//! before the fall-through can reach here. Unit tests run on the `Fake`
//! variant (a daemon stand-in; see [`super::fake_terminal`]).

#[cfg(test)]
use std::time::Duration;

use bytes::Bytes;
use ratatui::{layout::Rect, Frame};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use tokio::sync::mpsc;

#[cfg(test)]
use crate::layout::PaneId;
use crate::protocol::CellData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollMetrics {
    pub offset_from_bottom: usize,
    pub max_offset_from_bottom: usize,
    pub viewport_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    /// DECSCUSR parameter (0–6). 0 means terminal default.
    pub shape: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalDirtyPatch {
    pub rows: Vec<(u16, Vec<CellData>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalDirtyPatchOutcome {
    Clean,
    Patch(TerminalDirtyPatch),
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputState {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_protocol_mode: crate::input::MouseProtocolMode,
    pub mouse_protocol_encoding: crate::input::MouseProtocolEncoding,
    pub mouse_alternate_scroll: bool,
    #[serde(default)]
    pub modify_other_keys: bool,
}

impl InputState {
    pub fn mouse_reporting_enabled(self) -> bool {
        self.mouse_protocol_mode.reporting_enabled()
    }
}

/// Test-only since WS0 stage C: prod panes have no local byte stream (the Go
/// daemon owns the PTY); only the fake terminal feeds bytes.
#[cfg(test)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProcessBytesResult {
    pub request_render: bool,
    pub render_delay: Option<Duration>,
    pub clipboard_writes: Vec<Vec<u8>>,
    pub reported_cwd: Option<std::path::PathBuf>,
    pub terminal_responses: Vec<Bytes>,
}

/// A pane's local terminal-side state: mirrored input modes + pure encoders
/// for a live termhost pane, or the test double.
pub(crate) enum PaneTerminal {
    Mirror(super::input_mirror::InputMirror),
    #[cfg(test)]
    Fake(super::fake_terminal::FakePaneTerminal),
}

type Mirror = super::input_mirror::InputMirror;

impl PaneTerminal {
    #[cfg_attr(test, allow(dead_code))] // real spawn tail is cfg'd out of test builds
    pub(crate) fn new_mirror() -> Self {
        Self::Mirror(Mirror::new())
    }

    #[cfg(test)]
    pub(crate) fn new_fake(cols: u16, rows: u16, scrollback_limit_bytes: usize) -> Self {
        Self::Fake(super::fake_terminal::FakePaneTerminal::new(
            cols,
            rows,
            scrollback_limit_bytes,
        ))
    }

    #[cfg(test)]
    fn fake(&self) -> Option<&super::fake_terminal::FakePaneTerminal> {
        match self {
            Self::Fake(fake) => Some(fake),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The input-mode mirror answering state/encoder queries: the prod mirror,
    /// or the fake's embedded mirror (same type, same code paths).
    fn input_mirror(&self) -> &super::input_mirror::InputMirror {
        match self {
            Self::Mirror(mirror) => mirror,
            #[cfg(test)]
            Self::Fake(fake) => fake.mirror(),
        }
    }

    #[cfg(test)]
    pub fn process_pty_bytes(
        &self,
        _pane_id: PaneId,
        _shell_pid: u32,
        bytes: &[u8],
        _response_writer: &mpsc::Sender<Bytes>,
    ) -> ProcessBytesResult {
        if let Some(fake) = self.fake() {
            fake.feed(bytes);
            return ProcessBytesResult {
                request_render: true,
                ..ProcessBytesResult::default()
            };
        }
        // A live termhost pane has no local byte stream to process.
        let _ = bytes;
        ProcessBytesResult::default()
    }

    /// Local resize bookkeeping; the PTY/emulator resize happens in the Go
    /// daemon (via `PaneRuntimeIo::resize`). No query responses locally.
    pub fn resize(
        &self,
        rows: u16,
        cols: u16,
        _cell_width_px: u32,
        _cell_height_px: u32,
    ) -> Vec<Bytes> {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            fake.resize(rows, cols);
        }
        let _ = (rows, cols);
        Vec::new()
    }

    pub fn scroll_up(&self, lines: usize) {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            fake.scroll_up(lines);
        }
        let _ = lines;
    }

    pub fn scroll_down(&self, lines: usize) {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            fake.scroll_down(lines);
        }
        let _ = lines;
    }

    pub fn scroll_reset(&self) {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            fake.scroll_reset();
        }
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            fake.set_scroll_offset_from_bottom(lines);
        }
        let _ = lines;
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.scroll_metrics();
        }
        None
    }

    pub fn input_state(&self) -> Option<InputState> {
        self.input_mirror().input_state()
    }

    pub fn apply_input_modes(&self, modes: &crate::termhost::PaneInputModes) {
        match self {
            Self::Mirror(mirror) => mirror.apply_input_modes(modes),
            #[cfg(test)]
            Self::Fake(fake) => fake.apply_input_modes(modes),
        }
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.input_mirror().wheel_routing()
    }

    pub fn cursor_state(&self) -> Option<TerminalCursorState> {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.cursor_state();
        }
        None
    }

    pub fn synchronized_output_active(&self) -> bool {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.synchronized_output_active();
        }
        self.input_mirror().synchronized_output_active()
    }

    pub fn visible_text(&self) -> String {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.visible_text();
        }
        String::new()
    }

    pub fn visible_ansi(&self) -> String {
        // The fake reconstructs no styling; plain text is the contract tests
        // rely on (assertions are `contains`-style).
        self.visible_text()
    }

    pub fn detection_text(&self) -> String {
        self.visible_text()
    }

    pub fn recent_text(&self, lines: usize) -> String {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.recent_text(lines, false);
        }
        let _ = lines;
        String::new()
    }

    pub fn recent_ansi(&self, lines: usize) -> String {
        self.recent_text(lines)
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.recent_text(lines, true);
        }
        let _ = lines;
        String::new()
    }

    pub fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        self.recent_unwrapped_text(lines)
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            let ((anchor_row, anchor_col), (cursor_row, cursor_col)) = selection.ordered_cells();
            return fake.extract_selection_cells(anchor_row, anchor_col, cursor_row, cursor_col);
        }
        let _ = selection;
        None
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            let snapshot = fake.snapshot();
            let cursor = snapshot.cursor.clone();
            super::render_wire_frame(frame, area, show_cursor, &snapshot, cursor.as_ref());
        }
        let _ = (frame, area, show_cursor);
    }

    pub fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return super::wire_dirty_patch(
                fake.take_dirty(),
                || Some(fake.snapshot()),
                area_width,
                area_height,
            );
        }
        let _ = (area_width, area_height);
        // Mirror panes are answered by the Go-backend arm; if this is ever
        // reached, force the full-redraw path.
        TerminalDirtyPatchOutcome::Fallback
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        #[cfg(test)]
        if let Some(fake) = self.fake() {
            return fake.visible_hyperlinks(area.x, area.y, area.width, area.height);
        }
        let _ = area;
        Vec::new()
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::terminal::types::KittyImagePlacement>
    where
        F: FnMut(crate::terminal::types::KittyImageDescriptor) -> bool,
    {
        // Kitty graphics for termhost panes travel in FrameData::graphics;
        // no local placement store exists.
        let _ = needs_data;
        Vec::new()
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        // Theme defaults are applied by the Go-side emulator.
        let _ = theme;
    }

    /// OSC 0/2 + OSC 9;4 evidence for agent detection lived in the emulator;
    /// Go-side detection owns it now. Empty until richer termhost signals
    /// carry it (later workstream).
    pub fn agent_osc_title(&self) -> String {
        String::new()
    }

    pub fn agent_osc_progress(&self) -> String {
        String::new()
    }

    pub fn keyboard_protocol(
        &self,
        fallback: crate::input::KeyboardProtocol,
    ) -> crate::input::KeyboardProtocol {
        self.input_mirror().keyboard_protocol().unwrap_or(fallback)
    }

    #[cfg(unix)]
    pub fn kitty_keyboard_state_ansi(&self) -> Option<String> {
        self.input_mirror().kitty_keyboard_state_ansi()
    }

    pub fn encode_terminal_key(
        &self,
        key: crate::input::TerminalKey,
        protocol: crate::input::KeyboardProtocol,
    ) -> Vec<u8> {
        self.input_mirror().encode_terminal_key(key, protocol)
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.input_mirror()
            .encode_mouse_button(kind, column, row, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.input_mirror()
            .encode_mouse_motion(kind, column, row, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.input_mirror()
            .encode_mouse_wheel(kind, column, row, modifiers)
    }
}
