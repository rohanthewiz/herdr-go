//! Ghostty-free input-mode state for termhost panes (WS0 stage B2).
//!
//! A termhost pane's VT emulation lives in the Go daemon; the Rust side only
//! needs the pane's *input modes* (to route wheel events, gate mouse
//! reporting, bracket pastes, …) and a key/mouse *encoder* matching the
//! program's negotiated protocols. Before this module, both came from an
//! unfed in-process `GhosttyPaneTerminal` that existed purely as a mirror.
//! `InputMirror` replaces it with a plain-data copy of the modes the Go
//! daemon reports (`PaneSignal::Modes`) and the pure-Rust encoders in
//! `crate::input`.
//!
//! Parity with the ghostty-backed mirror is pinned by the differential tests
//! at the bottom of this file (which drive both against the same reported
//! modes) — they keep this honest until the in-process path is deleted (WS0
//! stage D) and later WS9 moves encoding to Go.

use std::sync::Mutex;

use crossterm::event::{KeyModifiers, MouseEventKind};

use super::kitty_keyboard::KittyKeyboardTracker;
use super::terminal::InputState;
use super::WheelRouting;

/// Wire codes for the reported mouse tracking mode (see
/// `termhost::PaneInputModes`): 0 off, 1 X10, 2 press+release,
/// 3 button-motion, 4 any-motion.
const MOUSE_MODE_OFF: u8 = 0;
const MOUSE_MODE_X10: u8 = 1;
const MOUSE_MODE_PRESS_RELEASE: u8 = 2;
const MOUSE_MODE_BUTTON_MOTION: u8 = 3;
const MOUSE_MODE_ANY_MOTION: u8 = 4;

/// Wire codes for the reported mouse encoding: 0 default (X10 bytes),
/// 1 UTF-8, 2 SGR.
const MOUSE_ENCODING_DEFAULT: u8 = 0;
const MOUSE_ENCODING_UTF8: u8 = 1;
const MOUSE_ENCODING_SGR: u8 = 2;

#[derive(Default)]
struct MirrorState {
    alternate_screen: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    focus_reporting: bool,
    mouse_mode: u8,
    mouse_encoding: u8,
    mouse_alternate_scroll: bool,
    synchronized_output: bool,
    /// XTMODKEYS modifyOtherKeys, only restorable from a handoff snapshot —
    /// the Go daemon does not report it (yet; revisit in WS9).
    modify_other_keys: bool,
    /// Kitty keyboard protocol register + push/pop stack, shared with the
    /// in-process path (pure Rust). Fed absolutely by reported modes and by
    /// observed handoff-replay sequences.
    kitty_keyboard: KittyKeyboardTracker,
}

/// Plain-data stand-in for the unfed local emulator of a termhost pane.
pub(crate) struct InputMirror {
    state: Mutex<MirrorState>,
}

impl InputMirror {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MirrorState::default()),
        }
    }

    /// Mirrors the Go backend's reported input modes. Idempotent; the kitty
    /// flags register is set absolutely (no stack growth), matching the
    /// ghostty-backed mirror's `CSI = flags ; 1 u` behavior.
    pub(crate) fn apply_input_modes(&self, modes: &crate::termhost::PaneInputModes) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.alternate_screen = modes.alternate_screen;
        state.application_cursor = modes.application_cursor;
        state.bracketed_paste = modes.bracketed_paste;
        state.focus_reporting = modes.focus_reporting;
        state.mouse_mode = modes.mouse_mode;
        state.mouse_encoding = modes.mouse_encoding;
        state.mouse_alternate_scroll = modes.mouse_alternate_scroll;
        state.synchronized_output = modes.synchronized_output;
        state
            .kitty_keyboard
            .observe(format!("\x1b[={};1u", modes.kitty_keyboard_flags).as_bytes());
    }

    /// Seeds modes from a handoff/restore snapshot (cf.
    /// `GhosttyPaneTerminal::seed_handoff_input_state`). Not yet wired into a
    /// production path — termhost panes resync modes from the Go daemon on
    /// attach; this is the hook for the stage-C5 handoff redefinition.
    #[allow(dead_code)]
    pub(crate) fn seed_handoff_input_state(&self, input_state: InputState) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.alternate_screen = input_state.alternate_screen;
        state.application_cursor = input_state.application_cursor;
        state.bracketed_paste = input_state.bracketed_paste;
        state.focus_reporting = input_state.focus_reporting;
        state.mouse_mode = match input_state.mouse_protocol_mode {
            crate::input::MouseProtocolMode::None => MOUSE_MODE_OFF,
            crate::input::MouseProtocolMode::Press => MOUSE_MODE_X10,
            crate::input::MouseProtocolMode::PressRelease => MOUSE_MODE_PRESS_RELEASE,
            crate::input::MouseProtocolMode::ButtonMotion => MOUSE_MODE_BUTTON_MOTION,
            crate::input::MouseProtocolMode::AnyMotion => MOUSE_MODE_ANY_MOTION,
        };
        state.mouse_encoding = match input_state.mouse_protocol_encoding {
            crate::input::MouseProtocolEncoding::Default => MOUSE_ENCODING_DEFAULT,
            crate::input::MouseProtocolEncoding::Utf8 => MOUSE_ENCODING_UTF8,
            crate::input::MouseProtocolEncoding::Sgr => MOUSE_ENCODING_SGR,
        };
        state.mouse_alternate_scroll = input_state.mouse_alternate_scroll;
        state.modify_other_keys = input_state.modify_other_keys;
    }

    /// Seeds the kitty keyboard register from a snapshot that only recorded
    /// the flags (cf. `GhosttyPaneTerminal::seed_keyboard_protocol_flags`).
    /// See [`Self::seed_handoff_input_state`] on wiring.
    #[allow(dead_code)]
    pub(crate) fn seed_keyboard_protocol_flags(&self, flags: u16) {
        if flags == 0 {
            return;
        }
        self.seed_keyboard_protocol_ansi(&format!("\x1b[>{flags}u"));
    }

    /// Replays a snapshot's kitty keyboard sequences (push/pop stack aware).
    /// See [`Self::seed_handoff_input_state`] on wiring.
    #[allow(dead_code)]
    pub(crate) fn seed_keyboard_protocol_ansi(&self, ansi: &str) {
        if ansi.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.kitty_keyboard.observe(ansi.as_bytes());
        }
    }

    pub(crate) fn keyboard_protocol(&self) -> Option<crate::input::KeyboardProtocol> {
        let state = self.state.lock().ok()?;
        Some(crate::input::KeyboardProtocol::from_kitty_flags(
            state.kitty_keyboard.flags(),
        ))
    }

    #[cfg(unix)]
    pub(crate) fn kitty_keyboard_state_ansi(&self) -> Option<String> {
        self.state.lock().ok()?.kitty_keyboard.replay_ansi()
    }

    pub(crate) fn input_state(&self) -> Option<InputState> {
        let state = self.state.lock().ok()?;
        Some(InputState {
            alternate_screen: state.alternate_screen,
            application_cursor: state.application_cursor,
            bracketed_paste: state.bracketed_paste,
            focus_reporting: state.focus_reporting,
            mouse_protocol_mode: mouse_protocol_mode(state.mouse_mode),
            mouse_protocol_encoding: mouse_protocol_encoding(state.mouse_encoding),
            mouse_alternate_scroll: state.mouse_alternate_scroll,
            modify_other_keys: state.modify_other_keys,
        })
    }

    pub(crate) fn wheel_routing(&self) -> Option<WheelRouting> {
        let state = self.state.lock().ok()?;
        Some(if state.mouse_mode != MOUSE_MODE_OFF {
            WheelRouting::MouseReport
        } else if state.alternate_screen && state.mouse_alternate_scroll {
            WheelRouting::AlternateScroll
        } else {
            WheelRouting::HostScroll
        })
    }

    pub(crate) fn synchronized_output_active(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.synchronized_output)
            .unwrap_or(false)
    }

    pub(crate) fn encode_terminal_key(
        &self,
        key: crate::input::TerminalKey,
        protocol: crate::input::KeyboardProtocol,
    ) -> Vec<u8> {
        let application_cursor = self
            .state
            .lock()
            .map(|state| state.application_cursor)
            .unwrap_or(false);
        crate::input::encode_terminal_key_with_modes(key, protocol, application_cursor)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) -> Option<Vec<u8>> {
        let (mode, encoding) = self.mouse_reporting()?;
        // Gate by tracking mode: X10 reports presses only; press+release adds
        // releases; button-motion adds drags.
        let allowed = match kind {
            MouseEventKind::Down(_) => mode >= MOUSE_MODE_X10,
            MouseEventKind::Up(_) => mode >= MOUSE_MODE_PRESS_RELEASE,
            MouseEventKind::Drag(_) => mode >= MOUSE_MODE_BUTTON_MOTION,
            _ => false,
        };
        if !allowed {
            return None;
        }
        // X10 reports carry no modifier bits.
        let modifiers = if mode == MOUSE_MODE_X10 {
            KeyModifiers::empty()
        } else {
            modifiers
        };
        crate::input::encode_mouse_button(kind, column, row, modifiers, encoding)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) -> Option<Vec<u8>> {
        let (mode, encoding) = self.mouse_reporting()?;
        if mode != MOUSE_MODE_ANY_MOTION || kind != MouseEventKind::Moved {
            return None;
        }
        crate::input::encode_mouse_moved(column, row, modifiers, encoding)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) -> Option<Vec<u8>> {
        let (mode, encoding) = self.mouse_reporting()?;
        // Wheel buttons (64+) postdate X10: the ghostty encoder only reports
        // them from press+release tracking (mode 1000) upward.
        if mode < MOUSE_MODE_PRESS_RELEASE {
            return None;
        }
        crate::input::encode_mouse_scroll(kind, column, row, modifiers, encoding)
    }

    /// The (mode, encoding) pair when mouse reporting is on; `None` when off.
    fn mouse_reporting(&self) -> Option<(u8, crate::input::MouseProtocolEncoding)> {
        let state = self.state.lock().ok()?;
        if state.mouse_mode == MOUSE_MODE_OFF {
            return None;
        }
        Some((
            state.mouse_mode,
            mouse_protocol_encoding(state.mouse_encoding),
        ))
    }
}

fn mouse_protocol_mode(mode: u8) -> crate::input::MouseProtocolMode {
    match mode {
        MOUSE_MODE_X10 => crate::input::MouseProtocolMode::Press,
        MOUSE_MODE_PRESS_RELEASE => crate::input::MouseProtocolMode::PressRelease,
        MOUSE_MODE_BUTTON_MOTION => crate::input::MouseProtocolMode::ButtonMotion,
        MOUSE_MODE_ANY_MOTION => crate::input::MouseProtocolMode::AnyMotion,
        _ => crate::input::MouseProtocolMode::None,
    }
}

fn mouse_protocol_encoding(encoding: u8) -> crate::input::MouseProtocolEncoding {
    match encoding {
        MOUSE_ENCODING_UTF8 => crate::input::MouseProtocolEncoding::Utf8,
        MOUSE_ENCODING_SGR => crate::input::MouseProtocolEncoding::Sgr,
        _ => crate::input::MouseProtocolEncoding::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termhost::PaneInputModes;

    /// Builds the ghostty-backed mirror (an unfed emulator) the way termhost
    /// panes did before B2, so we can differentially test parity.
    fn ghostty_mirror() -> super::super::terminal::PaneTerminal {
        let (response_tx, _rx) = tokio::sync::mpsc::channel(1);
        let terminal = crate::ghostty::Terminal::new(80, 24, 10_000).unwrap();
        let ghostty =
            super::super::terminal::GhosttyPaneTerminal::new(terminal, response_tx).unwrap();
        super::super::terminal::PaneTerminal::new(ghostty)
    }

    fn modes(mouse_mode: u8, mouse_encoding: u8, kitty_flags: u16) -> PaneInputModes {
        PaneInputModes {
            alternate_screen: mouse_mode.is_multiple_of(2),
            application_cursor: mouse_mode >= 2,
            bracketed_paste: true,
            focus_reporting: mouse_mode >= 1,
            mouse_mode,
            mouse_encoding,
            mouse_alternate_scroll: mouse_mode <= 2,
            synchronized_output: mouse_mode == 3,
            kitty_keyboard_flags: kitty_flags,
        }
    }

    fn key(
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> crate::input::TerminalKey {
        crossterm::event::KeyEvent::new(code, mods).into()
    }

    #[test]
    fn mirror_matches_ghostty_mirror_state_and_encodings() {
        use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

        let key_matrix = [
            key(KeyCode::Up, KeyModifiers::empty()),
            key(KeyCode::Down, KeyModifiers::empty()),
            key(KeyCode::Home, KeyModifiers::empty()),
            key(KeyCode::Up, KeyModifiers::SHIFT),
            key(KeyCode::Enter, KeyModifiers::empty()),
            key(KeyCode::Backspace, KeyModifiers::empty()),
            key(KeyCode::Esc, KeyModifiers::empty()),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Tab, KeyModifiers::empty()),
            key(KeyCode::BackTab, KeyModifiers::SHIFT),
            key(KeyCode::F(5), KeyModifiers::empty()),
            key(KeyCode::PageUp, KeyModifiers::empty()),
            key(KeyCode::Delete, KeyModifiers::CONTROL),
        ];
        let mouse_matrix = [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ];
        let wheel_matrix = [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown];

        // Kitty flag coverage: 0 (off), 1 (disambiguate), 5 (disambiguate +
        // report-alternate-keys) — the sets real programs push. Bits 2/8
        // (report-event-types / report-all-keys-as-escape-codes) are a known
        // divergence: the pure encoder degrades them to legacy-compatible
        // output, as herdr's ghostty fallback path always has for text keys.
        // WS9 (key encoding in Go) is where full protocol coverage lands.
        for mouse_mode in 0..=4u8 {
            for mouse_encoding in 0..=2u8 {
                for kitty_flags in [0u16, 1, 5] {
                    let reported = modes(mouse_mode, mouse_encoding, kitty_flags);
                    let ghostty = ghostty_mirror();
                    ghostty.apply_input_modes(&reported);
                    let mirror = InputMirror::new();
                    mirror.apply_input_modes(&reported);
                    let case = format!(
                        "mode={mouse_mode} encoding={mouse_encoding} kitty={kitty_flags}"
                    );

                    // Mode state parity.
                    let g_state = ghostty.input_state().unwrap();
                    let m_state = mirror.input_state().unwrap();
                    assert_eq!(g_state, m_state, "input_state {case}");
                    assert_eq!(
                        ghostty.wheel_routing(),
                        mirror.wheel_routing(),
                        "wheel_routing {case}"
                    );
                    assert_eq!(
                        ghostty.synchronized_output_active(),
                        mirror.synchronized_output_active(),
                        "synchronized_output {case}"
                    );
                    let fallback = crate::input::KeyboardProtocol::Legacy;
                    assert_eq!(
                        ghostty.keyboard_protocol(fallback),
                        mirror.keyboard_protocol().unwrap_or(fallback),
                        "keyboard_protocol {case}"
                    );

                    // Key encoding parity.
                    let protocol = mirror.keyboard_protocol().unwrap();
                    for key in key_matrix {
                        assert_eq!(
                            ghostty.encode_terminal_key(key, protocol),
                            mirror.encode_terminal_key(key, protocol),
                            "key {key:?} {case}"
                        );
                    }

                    // Mouse encoding parity.
                    for kind in mouse_matrix {
                        assert_eq!(
                            ghostty.encode_mouse_button(kind, 10, 5, KeyModifiers::empty()),
                            mirror.encode_mouse_button(kind, 10, 5, KeyModifiers::empty()),
                            "mouse {kind:?} {case}"
                        );
                        assert_eq!(
                            ghostty.encode_mouse_button(kind, 10, 5, KeyModifiers::SHIFT),
                            mirror.encode_mouse_button(kind, 10, 5, KeyModifiers::SHIFT),
                            "mouse+shift {kind:?} {case}"
                        );
                    }
                    assert_eq!(
                        ghostty.encode_mouse_motion(
                            MouseEventKind::Moved,
                            10,
                            5,
                            KeyModifiers::empty()
                        ),
                        mirror.encode_mouse_motion(
                            MouseEventKind::Moved,
                            10,
                            5,
                            KeyModifiers::empty()
                        ),
                        "motion {case}"
                    );
                    for kind in wheel_matrix {
                        assert_eq!(
                            ghostty.encode_mouse_wheel(kind, 10, 5, KeyModifiers::empty()),
                            mirror.encode_mouse_wheel(kind, 10, 5, KeyModifiers::empty()),
                            "wheel {kind:?} {case}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn handoff_seed_round_trips_kitty_state() {
        let mirror = InputMirror::new();
        mirror.seed_keyboard_protocol_ansi("\x1b[=1u\x1b[>5u");
        assert_eq!(
            mirror.keyboard_protocol(),
            Some(crate::input::KeyboardProtocol::Kitty { flags: 5 })
        );
        assert_eq!(
            mirror.kitty_keyboard_state_ansi().as_deref(),
            Some("\x1b[=1u\x1b[>5u")
        );

        let flags_only = InputMirror::new();
        flags_only.seed_keyboard_protocol_flags(3);
        assert_eq!(
            flags_only.keyboard_protocol(),
            Some(crate::input::KeyboardProtocol::Kitty { flags: 3 })
        );
    }
}
