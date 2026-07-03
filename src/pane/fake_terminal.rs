//! Test-only stand-in for the Go daemon's pane terminal (WS0 stage C6).
//!
//! Unit tests used to seed pane content by feeding VT bytes to the in-process
//! ghostty emulator. That emulator is gone; the prod source of truth is the Go
//! daemon, which reports frames/modes over the seam. This fake plays the
//! daemon's role for unit tests: a *deliberately tiny* VT interpreter — plain
//! text, line discipline, and exactly the escape sequences the test corpus
//! feeds (a mode whitelist, basic SGR, cursor addressing, OSC 8) — over a
//! wire-shaped grid that snapshots to [`crate::protocol::FrameData`]. Unknown
//! CSI finals and unknown DEC private modes panic, so a new test feeding an
//! unsupported sequence fails loudly instead of silently reading empty
//! content.
//!
//! Input-mode state and encoders delegate to the *real* [`InputMirror`], so
//! tests exercise the same mode/encoder code prod termhost panes use.

use std::sync::Mutex;

use super::input_mirror::InputMirror;
use super::terminal::{InputState, ScrollMetrics, TerminalCursorState};
use crate::protocol::{CellData, CursorState, FrameData};

/// Mouse tracking / encoding wire codes (see `termhost::PaneInputModes`).
const MOUSE_OFF: u8 = 0;
const MOUSE_X10: u8 = 1;
const MOUSE_PRESS_RELEASE: u8 = 2;
const MOUSE_BUTTON_MOTION: u8 = 3;
const MOUSE_ANY_MOTION: u8 = 4;
const ENC_UTF8: u8 = 1;
const ENC_SGR: u8 = 2;

#[derive(Clone, Default)]
struct FakeCell {
    /// Empty symbol + `skip` marks a wide-char continuation cell.
    symbol: String,
    fg: u32,
    bg: u32,
    modifier: u16,
    skip: bool,
    uri: Option<String>,
}

impl FakeCell {
    fn blank() -> Self {
        FakeCell {
            symbol: " ".to_string(),
            ..FakeCell::default()
        }
    }

    fn to_wire(&self, uri_index: Option<u32>) -> CellData {
        // Wide-char continuation cells are emitted as plain blanks: that is
        // what the ratatui-buffer -> FrameData conversion produces on the full
        // render path (and what the deleted ghostty patch emitted), so the
        // retained-patch parity contract holds. The internal `skip` flag is
        // kept for text extraction only.
        if self.skip {
            return CellData {
                symbol: " ".to_string(),
                fg: self.fg,
                bg: self.bg,
                modifier: self.modifier,
                skip: false,
                hyperlink: None,
            };
        }
        CellData {
            symbol: self.symbol.clone(),
            fg: self.fg,
            bg: self.bg,
            modifier: self.modifier,
            skip: false,
            hyperlink: uri_index,
        }
    }
}

#[derive(Clone)]
struct Row {
    cells: Vec<FakeCell>,
    /// True when this row is a soft-wrap continuation of the previous row.
    wrapped: bool,
}

impl Row {
    fn blank(cols: u16) -> Self {
        Row {
            cells: vec![FakeCell::blank(); cols as usize],
            wrapped: false,
        }
    }

    /// Plain text of the row, trailing whitespace trimmed. Skip cells (wide
    /// char continuations) contribute nothing.
    fn text(&self) -> String {
        let mut out = String::new();
        for cell in &self.cells {
            if !cell.skip {
                out.push_str(&cell.symbol);
            }
        }
        out.trim_end().to_string()
    }

    /// Text between two display columns, `start..=end` inclusive, trailing
    /// whitespace trimmed.
    fn text_between(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if i < start || i > end || cell.skip {
                continue;
            }
            out.push_str(&cell.symbol);
        }
        out.trim_end().to_string()
    }
}

/// Full copy of the mode state the parser maintains; re-seeded into the
/// [`InputMirror`] after every change (the mirror's seed API sets all fields
/// at once).
#[derive(Clone, Copy, Default)]
struct Modes {
    alternate_screen: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    focus_reporting: bool,
    mouse_mode: u8,
    mouse_encoding: u8,
    mouse_alternate_scroll: bool,
    synchronized_output: bool,
    modify_other_keys: bool,
}

struct Screen {
    cols: u16,
    rows: u16,
    /// 0 disables scrollback entirely; otherwise an approximate byte budget
    /// (sum of retained row text lengths), matching how tests sized the old
    /// emulator's scrollback.
    scrollback_limit_bytes: usize,
    /// Rows scrolled off the top of the primary screen (absolute rows 0..n).
    scrollback: Vec<Row>,
    primary: Vec<Row>,
    alt: Vec<Row>,
    modes: Modes,
    /// Cursor position within the active grid (0-based).
    cur_row: u16,
    cur_col: u16,
    saved_cursor: (u16, u16),
    cursor_visible: bool,
    cursor_shape: u8,
    /// Current SGR state applied to newly written cells.
    fg: u32,
    bg: u32,
    modifier: u16,
    /// Open OSC 8 hyperlink applied to newly written cells.
    hyperlink: Option<String>,
    /// Scrollback offset (0 = live bottom).
    offset_from_bottom: usize,
    dirty: bool,
}

impl Screen {
    fn new(cols: u16, rows: u16, scrollback_limit_bytes: usize) -> Self {
        let blank: Vec<Row> = (0..rows).map(|_| Row::blank(cols)).collect();
        Screen {
            cols,
            rows,
            scrollback_limit_bytes,
            scrollback: Vec::new(),
            primary: blank.clone(),
            alt: blank,
            modes: Modes::default(),
            cur_row: 0,
            cur_col: 0,
            saved_cursor: (0, 0),
            cursor_visible: true,
            cursor_shape: 0,
            fg: 0,
            bg: 0,
            modifier: 0,
            hyperlink: None,
            offset_from_bottom: 0,
            dirty: false,
        }
    }

    fn active(&self) -> &Vec<Row> {
        if self.modes.alternate_screen {
            &self.alt
        } else {
            &self.primary
        }
    }

    fn active_mut(&mut self) -> &mut Vec<Row> {
        if self.modes.alternate_screen {
            &mut self.alt
        } else {
            &mut self.primary
        }
    }

    /// All addressable rows: scrollback then the active screen. The alternate
    /// screen has no scrollback (matching terminal semantics).
    fn all_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if !self.modes.alternate_screen {
            rows.extend(self.scrollback.iter().cloned());
        }
        rows.extend(self.active().iter().cloned());
        rows
    }

    /// The `self.rows`-sized window ending `offset_from_bottom` rows above
    /// the live bottom, padded with blank rows if the buffer is shorter.
    fn viewport(&self) -> Vec<Row> {
        let all = self.all_rows();
        let end = all.len().saturating_sub(self.offset_from_bottom);
        let start = end.saturating_sub(self.rows as usize);
        let mut window: Vec<Row> = all[start..end].to_vec();
        while window.len() < self.rows as usize {
            window.push(Row::blank(self.cols));
        }
        window
    }

    fn max_offset(&self) -> usize {
        if self.modes.alternate_screen {
            0
        } else {
            self.scrollback.len()
        }
    }

    fn newline(&mut self, wrapped: bool) {
        if (self.cur_row as usize) + 1 < self.rows as usize {
            self.cur_row += 1;
        } else {
            // Scroll: the top active row moves into scrollback (primary only).
            let cols = self.cols;
            let limit = self.scrollback_limit_bytes;
            let alternate = self.modes.alternate_screen;
            let grid = self.active_mut();
            let evicted = if grid.is_empty() {
                Row::blank(cols)
            } else {
                grid.remove(0)
            };
            grid.push(Row::blank(cols));
            if !alternate && limit > 0 {
                self.scrollback.push(evicted);
                let mut used: usize = self.scrollback.iter().map(|r| r.text().len() + 1).sum();
                while used > limit && !self.scrollback.is_empty() {
                    used -= self.scrollback[0].text().len() + 1;
                    self.scrollback.remove(0);
                }
            }
        }
        let row = self.cur_row as usize;
        if let Some(row) = self.active_mut().get_mut(row) {
            row.wrapped = wrapped;
        }
    }

    fn put_char(&mut self, ch: char) {
        use unicode_width::UnicodeWidthChar;
        let width = (ch.width().unwrap_or(0).max(1)) as u16;
        if self.cur_col + width > self.cols {
            self.newline(true);
            self.cur_col = 0;
        }
        let cell = FakeCell {
            symbol: ch.to_string(),
            fg: self.fg,
            bg: self.bg,
            modifier: self.modifier,
            skip: false,
            uri: self.hyperlink.clone(),
        };
        let (cur_row, cur_col) = (self.cur_row as usize, self.cur_col as usize);
        let skip_cell = FakeCell {
            symbol: String::new(),
            fg: self.fg,
            bg: self.bg,
            modifier: self.modifier,
            skip: true,
            uri: None,
        };
        let grid = self.active_mut();
        if let Some(row) = grid.get_mut(cur_row) {
            if let Some(slot) = row.cells.get_mut(cur_col) {
                *slot = cell;
            }
            if width == 2 {
                if let Some(slot) = row.cells.get_mut(cur_col + 1) {
                    *slot = skip_cell;
                }
            }
        }
        self.cur_col += width;
    }

    fn clear_active(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let grid = self.active_mut();
        grid.clear();
        for _ in 0..rows {
            grid.push(Row::blank(cols));
        }
    }
}

/// Test double for a pane terminal: VT-lite content model + the real
/// [`InputMirror`] for modes and encoders.
pub(crate) struct FakePaneTerminal {
    mirror: InputMirror,
    screen: Mutex<Screen>,
}

impl FakePaneTerminal {
    pub(crate) fn new(cols: u16, rows: u16, scrollback_limit_bytes: usize) -> Self {
        FakePaneTerminal {
            mirror: InputMirror::new(),
            screen: Mutex::new(Screen::new(
                cols.max(1),
                rows.max(1),
                scrollback_limit_bytes,
            )),
        }
    }

    // --- feeding ---------------------------------------------------------

    /// Interprets a byte chunk as the daemon-side emulator would, updating
    /// grid content and mode state. Panics on sequences outside the
    /// supported whitelist.
    pub(crate) fn feed(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let mut screen = self.screen.lock().unwrap();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut prefix = None;
                        let mut params = String::new();
                        let mut intermediate = None;
                        let mut fin = None;
                        if matches!(chars.peek(), Some('?' | '>' | '=' | '<')) {
                            prefix = chars.next();
                        }
                        for c in chars.by_ref() {
                            match c {
                                '0'..='9' | ';' => params.push(c),
                                ' ' => intermediate = Some(' '),
                                _ => {
                                    fin = Some(c);
                                    break;
                                }
                            }
                        }
                        let Some(fin) = fin else {
                            panic!("fake terminal: unterminated CSI sequence");
                        };
                        Self::apply_csi(
                            &self.mirror,
                            &mut screen,
                            prefix,
                            &params,
                            intermediate,
                            fin,
                        );
                    }
                    Some(']') => {
                        // OSC: collect until BEL or ST (ESC \).
                        let mut body = String::new();
                        loop {
                            match chars.next() {
                                Some('\x07') | None => break,
                                Some('\x1b') => {
                                    if chars.peek() == Some(&'\\') {
                                        chars.next();
                                    }
                                    break;
                                }
                                Some(c) => body.push(c),
                            }
                        }
                        Self::apply_osc(&mut screen, &body);
                    }
                    other => panic!("fake terminal: unsupported escape {other:?}"),
                },
                '\r' => screen.cur_col = 0,
                '\n' => screen.newline(false),
                '\x08' => screen.cur_col = screen.cur_col.saturating_sub(1),
                '\x07' => {}
                '\t' => {
                    let next_stop = ((screen.cur_col / 8) + 1) * 8;
                    screen.cur_col = next_stop.min(screen.cols.saturating_sub(1));
                }
                _ => screen.put_char(ch),
            }
        }
        screen.dirty = true;
        drop(screen);
        self.sync_mirror();
    }

    fn apply_csi(
        mirror: &InputMirror,
        screen: &mut Screen,
        prefix: Option<char>,
        params: &str,
        intermediate: Option<char>,
        fin: char,
    ) {
        let nums: Vec<u16> = params
            .split(';')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        match (prefix, intermediate, fin) {
            // Kitty keyboard protocol: replay the raw sequence into the real
            // tracker (push/pop/set aware).
            (Some(p @ ('>' | '=' | '<')), None, 'u') => {
                mirror.seed_keyboard_protocol_ansi(&format!("\x1b[{p}{params}u"));
            }
            // XTMODKEYS modifyOtherKeys.
            (Some('>'), None, 'm') => {
                if nums.first() == Some(&4) {
                    screen.modes.modify_other_keys = nums.get(1).copied().unwrap_or(0) > 0;
                }
            }
            (Some('?'), None, 'h') | (Some('?'), None, 'l') => {
                let set = fin == 'h';
                for mode in &nums {
                    Self::apply_dec_mode(screen, *mode, set);
                }
            }
            (None, None, 'm') => Self::apply_sgr(screen, &nums),
            (None, None, 'H') | (None, None, 'f') => {
                let row = nums.first().copied().unwrap_or(1).max(1) - 1;
                let col = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                screen.cur_row = row.min(screen.rows.saturating_sub(1));
                screen.cur_col = col.min(screen.cols.saturating_sub(1));
            }
            (None, None, 'A') => {
                screen.cur_row = screen
                    .cur_row
                    .saturating_sub(nums.first().copied().unwrap_or(1).max(1));
            }
            (None, None, 'B') => {
                let n = nums.first().copied().unwrap_or(1).max(1);
                screen.cur_row = (screen.cur_row + n).min(screen.rows.saturating_sub(1));
            }
            (None, None, 'C') => {
                let n = nums.first().copied().unwrap_or(1).max(1);
                screen.cur_col = (screen.cur_col + n).min(screen.cols.saturating_sub(1));
            }
            (None, None, 'D') => {
                screen.cur_col = screen
                    .cur_col
                    .saturating_sub(nums.first().copied().unwrap_or(1).max(1));
            }
            (None, Some(' '), 'q') => {
                screen.cursor_shape = nums.first().copied().unwrap_or(0) as u8;
            }
            (None, None, 'J') => screen.clear_active(),
            (None, None, 'K') => {
                let (row, col) = (screen.cur_row as usize, screen.cur_col as usize);
                if let Some(row) = screen.active_mut().get_mut(row) {
                    for cell in row.cells.iter_mut().skip(col) {
                        *cell = FakeCell::blank();
                    }
                }
            }
            other => panic!(
                "fake terminal: unsupported CSI {:?} params={params:?} — extend \
                 pane/fake_terminal.rs if a test legitimately needs it",
                other
            ),
        }
    }

    fn apply_dec_mode(screen: &mut Screen, mode: u16, set: bool) {
        let modes = &mut screen.modes;
        match mode {
            1 => modes.application_cursor = set,
            9 => modes.mouse_mode = if set { MOUSE_X10 } else { MOUSE_OFF },
            12 => {} // cursor blink — irrelevant to tests
            25 => screen.cursor_visible = set,
            47 | 1047 => {
                modes.alternate_screen = set;
                if set {
                    screen.clear_active();
                }
            }
            1048 => {
                if set {
                    screen.saved_cursor = (screen.cur_row, screen.cur_col);
                } else {
                    (screen.cur_row, screen.cur_col) = screen.saved_cursor;
                }
            }
            1049 => {
                if set {
                    screen.saved_cursor = (screen.cur_row, screen.cur_col);
                    modes.alternate_screen = true;
                    screen.clear_active();
                    screen.cur_row = 0;
                    screen.cur_col = 0;
                } else {
                    modes.alternate_screen = false;
                    (screen.cur_row, screen.cur_col) = screen.saved_cursor;
                }
            }
            1000 => modes.mouse_mode = if set { MOUSE_PRESS_RELEASE } else { MOUSE_OFF },
            1002 => modes.mouse_mode = if set { MOUSE_BUTTON_MOTION } else { MOUSE_OFF },
            1003 => modes.mouse_mode = if set { MOUSE_ANY_MOTION } else { MOUSE_OFF },
            1004 => modes.focus_reporting = set,
            1005 => modes.mouse_encoding = if set { ENC_UTF8 } else { 0 },
            // SGR (1006) and SGR-pixel (1016) both degrade to SGR cell
            // encoding here — the pure encoders never produce pixel reports.
            1006 | 1016 => modes.mouse_encoding = if set { ENC_SGR } else { 0 },
            1007 => modes.mouse_alternate_scroll = set,
            2004 => modes.bracketed_paste = set,
            2026 => modes.synchronized_output = set,
            other => panic!(
                "fake terminal: unsupported DEC private mode {other} — extend \
                 pane/fake_terminal.rs if a test legitimately needs it"
            ),
        }
    }

    fn apply_sgr(screen: &mut Screen, nums: &[u16]) {
        // Unknown SGR params are ignored (styling only, never load-bearing
        // for behavior); recognized ones map onto the wire color encoding.
        if nums.is_empty() {
            screen.fg = 0;
            screen.bg = 0;
            screen.modifier = 0;
            return;
        }
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => {
                    screen.fg = 0;
                    screen.bg = 0;
                    screen.modifier = 0;
                }
                1 => screen.modifier |= ratatui::style::Modifier::BOLD.bits(),
                3 => screen.modifier |= ratatui::style::Modifier::ITALIC.bits(),
                4 => screen.modifier |= ratatui::style::Modifier::UNDERLINED.bits(),
                7 => screen.modifier |= ratatui::style::Modifier::REVERSED.bits(),
                30..=37 => screen.fg = (nums[i] - 30 + 1) as u32,
                39 => screen.fg = 0,
                40..=47 => screen.bg = (nums[i] - 40 + 1) as u32,
                49 => screen.bg = 0,
                38 | 48 => {
                    // 38;5;n / 48;5;n indexed, 38;2;r;g;b / 48;2;r;g;b rgb
                    let target_fg = nums[i] == 38;
                    let value = match nums.get(i + 1) {
                        Some(5) => {
                            let idx = nums.get(i + 2).copied().unwrap_or(0) as u32;
                            i += 2;
                            0x01_00_00_00 | idx
                        }
                        Some(2) => {
                            let r = nums.get(i + 2).copied().unwrap_or(0) as u32;
                            let g = nums.get(i + 3).copied().unwrap_or(0) as u32;
                            let b = nums.get(i + 4).copied().unwrap_or(0) as u32;
                            i += 4;
                            0x02_00_00_00 | (r << 16) | (g << 8) | b
                        }
                        _ => 0,
                    };
                    if target_fg {
                        screen.fg = value;
                    } else {
                        screen.bg = value;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn apply_osc(screen: &mut Screen, body: &str) {
        if let Some(rest) = body.strip_prefix("8;") {
            // OSC 8 hyperlink: "params;uri" — empty uri closes the link.
            let uri = rest.split_once(';').map(|(_, uri)| uri).unwrap_or("");
            screen.hyperlink = (!uri.is_empty()).then(|| uri.to_string());
        }
        // Other OSCs (title, cwd, clipboard) are daemon signals in prod, not
        // grid content — ignored here.
    }

    /// Pushes the parser's mode state into the mirror so input_state /
    /// encoders answer through the exact prod code path.
    fn sync_mirror(&self) {
        let modes = self.screen.lock().unwrap().modes;
        self.mirror.seed_handoff_input_state(InputState {
            alternate_screen: modes.alternate_screen,
            application_cursor: modes.application_cursor,
            bracketed_paste: modes.bracketed_paste,
            focus_reporting: modes.focus_reporting,
            mouse_protocol_mode: match modes.mouse_mode {
                MOUSE_X10 => crate::input::MouseProtocolMode::Press,
                MOUSE_PRESS_RELEASE => crate::input::MouseProtocolMode::PressRelease,
                MOUSE_BUTTON_MOTION => crate::input::MouseProtocolMode::ButtonMotion,
                MOUSE_ANY_MOTION => crate::input::MouseProtocolMode::AnyMotion,
                _ => crate::input::MouseProtocolMode::None,
            },
            mouse_protocol_encoding: match modes.mouse_encoding {
                ENC_UTF8 => crate::input::MouseProtocolEncoding::Utf8,
                ENC_SGR => crate::input::MouseProtocolEncoding::Sgr,
                _ => crate::input::MouseProtocolEncoding::Default,
            },
            mouse_alternate_scroll: modes.mouse_alternate_scroll,
            modify_other_keys: modes.modify_other_keys,
        });
        self.mirror
            .set_synchronized_output(modes.synchronized_output);
    }

    // --- mirror delegation -------------------------------------------------

    pub(crate) fn mirror(&self) -> &InputMirror {
        &self.mirror
    }

    /// Prod signal path (`PaneSignal::Modes`): adopt the daemon-reported modes
    /// wholesale, exactly like a live termhost pane.
    pub(crate) fn apply_input_modes(&self, modes: &crate::termhost::PaneInputModes) {
        {
            let mut screen = self.screen.lock().unwrap();
            screen.modes = Modes {
                alternate_screen: modes.alternate_screen,
                application_cursor: modes.application_cursor,
                bracketed_paste: modes.bracketed_paste,
                focus_reporting: modes.focus_reporting,
                mouse_mode: modes.mouse_mode,
                mouse_encoding: modes.mouse_encoding,
                mouse_alternate_scroll: modes.mouse_alternate_scroll,
                synchronized_output: modes.synchronized_output,
                modify_other_keys: modes.modify_other_keys,
            };
        }
        self.mirror.apply_input_modes(modes);
    }

    // --- content queries ----------------------------------------------------

    pub(crate) fn resize(&self, rows: u16, cols: u16) {
        let mut guard = self.screen.lock().unwrap();
        let screen = &mut *guard;
        screen.rows = rows.max(1);
        screen.cols = cols.max(1);
        for grid in [&mut screen.primary, &mut screen.alt] {
            for row in grid.iter_mut() {
                row.cells.resize(cols.max(1) as usize, FakeCell::blank());
            }
            while grid.len() < rows.max(1) as usize {
                grid.push(Row::blank(cols.max(1)));
            }
            while grid.len() > rows.max(1) as usize {
                grid.pop();
            }
        }
        screen.cur_row = screen.cur_row.min(rows.saturating_sub(1));
        screen.cur_col = screen.cur_col.min(cols.saturating_sub(1));
        // A resize interrupts synchronized-output batching, like the emulator did.
        screen.modes.synchronized_output = false;
        screen.dirty = true;
        drop(guard);
        self.sync_mirror();
    }

    pub(crate) fn visible_text(&self) -> String {
        let screen = self.screen.lock().unwrap();
        let mut lines: Vec<String> = screen.viewport().iter().map(Row::text).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Recent lines (scrollback + screen), trailing blank rows dropped.
    /// `unwrap` merges soft-wrapped continuation rows into one line.
    pub(crate) fn recent_text(&self, lines: usize, unwrap: bool) -> String {
        let screen = self.screen.lock().unwrap();
        let mut rows = screen.all_rows();
        while rows.last().is_some_and(|row| row.text().is_empty()) {
            rows.pop();
        }
        let mut out: Vec<String> = Vec::new();
        for row in &rows {
            if unwrap && row.wrapped {
                if let Some(last) = out.last_mut() {
                    last.push_str(&row.text());
                    continue;
                }
            }
            out.push(row.text());
        }
        let start = out.len().saturating_sub(lines);
        out[start..].join("\n")
    }

    pub(crate) fn scroll_up(&self, lines: usize) {
        let mut screen = self.screen.lock().unwrap();
        screen.offset_from_bottom = (screen.offset_from_bottom + lines).min(screen.max_offset());
        screen.dirty = true;
    }

    pub(crate) fn scroll_down(&self, lines: usize) {
        let mut screen = self.screen.lock().unwrap();
        screen.offset_from_bottom = screen.offset_from_bottom.saturating_sub(lines);
        screen.dirty = true;
    }

    pub(crate) fn scroll_reset(&self) {
        self.screen.lock().unwrap().offset_from_bottom = 0;
    }

    pub(crate) fn set_scroll_offset_from_bottom(&self, lines: usize) {
        let mut screen = self.screen.lock().unwrap();
        screen.offset_from_bottom = lines.min(screen.max_offset());
        screen.dirty = true;
    }

    pub(crate) fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        let screen = self.screen.lock().unwrap();
        Some(ScrollMetrics {
            offset_from_bottom: screen.offset_from_bottom,
            max_offset_from_bottom: screen.max_offset(),
            viewport_rows: screen.rows as usize,
        })
    }

    pub(crate) fn cursor_state(&self) -> Option<TerminalCursorState> {
        let screen = self.screen.lock().unwrap();
        Some(TerminalCursorState {
            x: screen.cur_col,
            y: screen.cur_row,
            visible: screen.cursor_visible && screen.offset_from_bottom == 0,
            shape: screen.cursor_shape,
        })
    }

    pub(crate) fn synchronized_output_active(&self) -> bool {
        self.screen.lock().unwrap().modes.synchronized_output
    }

    /// Wire-shaped snapshot of the current viewport, as the daemon would
    /// report it.
    pub(crate) fn snapshot(&self) -> FrameData {
        let screen = self.screen.lock().unwrap();
        let viewport = screen.viewport();
        let mut hyperlinks: Vec<String> = Vec::new();
        let mut cells = Vec::with_capacity(screen.cols as usize * screen.rows as usize);
        for row in &viewport {
            for cell in &row.cells {
                let uri_index =
                    cell.uri
                        .as_ref()
                        .map(|uri| match hyperlinks.iter().position(|u| u == uri) {
                            Some(i) => i as u32,
                            None => {
                                hyperlinks.push(uri.clone());
                                (hyperlinks.len() - 1) as u32
                            }
                        });
                cells.push(cell.to_wire(uri_index));
            }
        }
        FrameData {
            cells,
            width: screen.cols,
            height: screen.rows,
            cursor: Some(CursorState {
                x: screen.cur_col,
                y: screen.cur_row,
                visible: screen.cursor_visible && screen.offset_from_bottom == 0,
                shape: screen.cursor_shape,
            }),
            hyperlinks,
            graphics: Vec::new(),
        }
    }

    pub(crate) fn take_dirty(&self) -> bool {
        let mut screen = self.screen.lock().unwrap();
        std::mem::take(&mut screen.dirty)
    }

    /// Hyperlinked cells within the given pane-relative area, mirroring
    /// `TermhostPane::visible_hyperlinks`: `((x, y), cell_symbol, uri)` with
    /// x/y offset by the area origin.
    pub(crate) fn visible_hyperlinks(
        &self,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) -> Vec<((u16, u16), String, String)> {
        let screen = self.screen.lock().unwrap();
        let viewport = screen.viewport();
        let mut out = Vec::new();
        for (y, row) in viewport.iter().enumerate().take(height as usize) {
            for (x, cell) in row.cells.iter().enumerate().take(width as usize) {
                if let Some(uri) = &cell.uri {
                    out.push((
                        (origin_x + x as u16, origin_y + y as u16),
                        cell.symbol.clone(),
                        uri.clone(),
                    ));
                }
            }
        }
        out
    }

    /// Selection text between two absolute buffer endpoints (row 0 = top of
    /// scrollback), inclusive of both cells, rows joined by newlines with
    /// trailing whitespace trimmed — ghostty-like semantics.
    pub(crate) fn extract_selection_cells(
        &self,
        anchor_row: u32,
        anchor_col: u16,
        cursor_row: u32,
        cursor_col: u16,
    ) -> Option<String> {
        let screen = self.screen.lock().unwrap();
        let rows = screen.all_rows();
        let ((start_row, start_col), (end_row, end_col)) =
            if (anchor_row, anchor_col) <= (cursor_row, cursor_col) {
                ((anchor_row, anchor_col), (cursor_row, cursor_col))
            } else {
                ((cursor_row, cursor_col), (anchor_row, anchor_col))
            };
        let mut out: Vec<String> = Vec::new();
        for row_idx in start_row..=end_row {
            let Some(row) = rows.get(row_idx as usize) else {
                break;
            };
            let start = if row_idx == start_row {
                start_col as usize
            } else {
                0
            };
            let end = if row_idx == end_row {
                end_col as usize
            } else {
                row.cells.len().saturating_sub(1)
            };
            out.push(row.text_between(start, end));
        }
        Some(out.join("\n"))
    }
}
