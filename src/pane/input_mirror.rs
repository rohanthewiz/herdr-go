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
//! Parity with the (now deleted) ghostty-backed mirror was pinned by a
//! differential test while both existed (WS0 stages B2..C); WS9 moves
//! encoding to Go.

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
    /// XTMODKEYS modifyOtherKeys, reported by the Go daemon's raw-stream
    /// scanner (WS0 stage C6).
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
        state.modify_other_keys = modes.modify_other_keys;
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

    /// Test hook: mirrors DEC 2026 as parsed by the fake terminal. Prod
    /// termhost panes get this via `apply_input_modes`.
    #[cfg(test)]
    pub(crate) fn set_synchronized_output(&self, active: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.synchronized_output = active;
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
        let (application_cursor, modify_other_keys) = self
            .state
            .lock()
            .map(|state| (state.application_cursor, state.modify_other_keys))
            .unwrap_or((false, false));
        // XTMODKEYS modifyOtherKeys: a modified Enter would collapse to a
        // bare CR under the legacy encoding, so emit xterm's CSI 27 form
        // (the shape the deleted ghostty encoder produced, pinned by the
        // shift-enter routing test). Other keys keep the pure encoding
        // until WS9 moves key encoding to Go.
        if modify_other_keys
            && matches!(protocol, crate::input::KeyboardProtocol::Legacy)
            && key.code == crossterm::event::KeyCode::Enter
            && !key.modifiers.is_empty()
        {
            let mods = key.modifiers;
            let mut modifier = 1u8;
            if mods.contains(KeyModifiers::SHIFT) {
                modifier += 1;
            }
            if mods.contains(KeyModifiers::ALT) {
                modifier += 2;
            }
            if mods.contains(KeyModifiers::CONTROL) {
                modifier += 4;
            }
            if modifier > 1 {
                return format!("\x1b[27;{modifier};13~").into_bytes();
            }
        }
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
