//! Client for the Go `termhost` daemon: connects over a Unix socket, runs a
//! reader thread that fans incoming `pane_frame`/`pane_exited` events into
//! per-pane state, and hands out [`TermhostPane`] handles that implement
//! [`TerminalBackend`].

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::protocol as wire;

use super::proto::{self, Command, Event};
use super::TerminalBackend;

/// A per-pane signal pushed from the Go backend (out-of-band from the cell grid):
/// OSC-derived state and detection results. Delivered to a per-pane [`SignalSink`].
#[derive(Debug, Clone)]
pub enum PaneSignal {
    /// Working directory reported via OSC 7.
    Cwd(String),
    /// Agent detection result (Go owns detection for termhost panes).
    Agent {
        /// Canonical agent label ("claude", "codex", …), or "" for a plain shell.
        agent: String,
        /// idle | working | blocked | unknown.
        state: String,
        visible_blocker: bool,
        visible_working: bool,
    },
    /// Clipboard write reported via OSC 52 (decoded bytes; empty is a clear).
    Clipboard(Vec<u8>),
    /// Window title reported via OSC 0/2 (empty is a title-clear).
    Title(String),
    /// Input-affecting DEC modes changed; the owner mirrors them onto its local
    /// emulator so key/mouse encoding and input routing match the program.
    Modes(PaneInputModes),
}

/// The pane's input-affecting DEC mode state, as reported by the Go backend.
/// `mouse_mode`/`mouse_encoding` are the raw wire codes (see [`proto::Event::PaneModes`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneInputModes {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_mode: u8,
    pub mouse_encoding: u8,
    pub mouse_alternate_scroll: bool,
    pub synchronized_output: bool,
    pub kitty_keyboard_flags: u16,
    /// xterm XTMODKEYS modifyOtherKeys (CSI >4;Nm).
    pub modify_other_keys: bool,
}

/// Per-pane callback the owner installs to receive [`PaneSignal`]s. Invoked on the
/// client reader thread, so it must be cheap and non-blocking.
pub type SignalSink = Box<dyn Fn(PaneSignal) + Send + Sync>;

/// Shared, reader-thread-updated state for one pane. The reader thread folds Go
/// frames into a full accumulated grid; the render path reads snapshots of it.
struct PaneState {
    grid: Mutex<PaneGrid>,
    exit: Mutex<Option<i32>>,
    /// Installed at creation; receives out-of-band pane signals. Never mutated.
    sink: Option<SignalSink>,
    /// One-shot for an in-flight blocking selection request: the reader thread
    /// hands the `pane_selection` reply text to the waiting caller. Selection
    /// requests are issued one at a time from the UI thread (which then blocks on
    /// the reply), so a single slot is enough and replies are FIFO over the socket.
    pending_selection: Mutex<Option<mpsc::Sender<String>>>,
    /// One-shot for an in-flight blocking text-extraction request (pane_text reply).
    /// Same single-outstanding/FIFO reasoning as pending_selection.
    pending_text: Mutex<Option<mpsc::Sender<String>>>,
}

/// Accumulated full grid for one pane. Go sends the full grid each frame with
/// `skip` marking unchanged cells (and `full` set on a complete redraw); we fold
/// those in so a snapshot is always a complete grid the compositor can splice.
#[derive(Default)]
struct PaneGrid {
    cols: u16,
    rows: u16,
    cells: Vec<wire::CellData>,
    cursor: Option<wire::CursorState>,
    /// OSC 8 URI table for the current grid. Frames carrying links are sent full
    /// (so the table and the cells' indices always replace together); link-free
    /// frames carry an empty table, which is correct since no cell references it.
    hyperlinks: Vec<String>,
    /// Latest scrollback position reported by the backend (None until a frame with
    /// scrollback history arrives).
    scroll: Option<proto::FrameScroll>,
    /// Set when a frame changed the grid; cleared by the render path.
    dirty: bool,
    /// True once at least one frame has been folded in.
    has_frame: bool,
}

impl PaneGrid {
    /// Folds one incoming frame into the accumulated grid.
    fn apply(&mut self, frame: proto::Frame) {
        let n = frame.cols as usize * frame.rows as usize;
        if self.cols != frame.cols || self.rows != frame.rows || self.cells.len() != n {
            // First frame or a resize: start from a blank grid of the new size.
            self.cells = vec![blank_cell(); n];
            self.cols = frame.cols;
            self.rows = frame.rows;
        }
        for (i, cell) in frame.cells.into_iter().enumerate() {
            if i >= n {
                break;
            }
            // `skip` means "unchanged, keep the prior cell"; full frames never skip.
            if !cell.skip {
                self.cells[i] = cell;
            }
        }
        self.cursor = frame.cursor;
        self.hyperlinks = frame.hyperlinks;
        // Scrollback position is updated only when the frame reports it (panes
        // without history omit it), so the last known position is retained.
        if frame.scroll.is_some() {
            self.scroll = frame.scroll;
        }
        self.dirty = true;
        self.has_frame = true;
    }

    /// Resolves the OSC 8 hyperlinks in the accumulated grid within the
    /// `width`×`height` window at screen origin (`origin_x`, `origin_y`), as
    /// `((screen_x, screen_y), cell_symbol, uri)` per linked cell — matching the
    /// in-process emulator's shape so the native-TUI click resolver works.
    fn visible_hyperlinks(
        &self,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) -> Vec<((u16, u16), String, String)> {
        if self.hyperlinks.is_empty() {
            return Vec::new();
        }
        let cols = self.cols;
        let mut links = Vec::new();
        for y in 0..height.min(self.rows) {
            for x in 0..width.min(cols) {
                let idx = y as usize * cols as usize + x as usize;
                let Some(cell) = self.cells.get(idx) else {
                    continue;
                };
                let Some(h) = cell.hyperlink else { continue };
                if let Some(uri) = self.hyperlinks.get(h as usize) {
                    links.push((
                        (origin_x + x, origin_y + y),
                        cell.symbol.clone(),
                        uri.clone(),
                    ));
                }
            }
        }
        links
    }

    fn snapshot(&self) -> Option<wire::FrameData> {
        if !self.has_frame {
            return None;
        }
        Some(wire::FrameData {
            cells: self.cells.clone(),
            width: self.cols,
            height: self.rows,
            cursor: self.cursor.clone(),
            hyperlinks: self.hyperlinks.clone(),
            graphics: Vec::new(),
        })
    }
}

fn blank_cell() -> wire::CellData {
    wire::CellData {
        symbol: " ".to_string(),
        fg: 0,
        bg: 0,
        modifier: 0,
        skip: false,
        hyperlink: None,
    }
}

/// A connection to the Go terminal backend. Owns the send side and a background
/// reader thread; hand out panes with [`TermhostClient::create_pane`].
pub struct TermhostClient {
    writer: Mutex<UnixStream>,
    panes: Mutex<HashMap<u32, Arc<PaneState>>>,
    /// Pane IDs the daemon already had live at connect (from welcome.panes). Empty
    /// on a fresh daemon; populated when we reconnect to a persistent daemon after a
    /// restart/handoff. Restore reconciles its session against this: a restored pane
    /// whose ID is here is adopted (not re-created).
    surviving_panes: Vec<u32>,
    /// Surviving pane IDs not yet claimed by an adoption. Each ID is adoptable
    /// exactly once: a later spawn with a recycled pane id (e.g. the shell
    /// respawn after the adopted process exits) must create a fresh daemon
    /// pane, not re-adopt the dead one.
    unclaimed_surviving: Mutex<Vec<u32>>,
}

/// Parameters for spawning a pane on the backend.
#[derive(Debug, Clone, Default)]
pub struct PaneSpec {
    pub pane_id: u32,
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub cwd: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// VT-encoded scrollback to seed before the child runs (restored history).
    pub initial_history: String,
}

impl TermhostClient {
    /// Connects to the daemon at `path`, performs the handshake, and starts the
    /// reader thread.
    pub fn connect(path: &str) -> io::Result<Arc<Self>> {
        let stream = UnixStream::connect(path)?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        proto::write_command(
            &mut writer,
            &Command::Hello {
                protocol_version: proto::PROTOCOL_VERSION,
            },
        )?;
        let surviving_panes = match proto::read_event(&mut reader)? {
            Event::Welcome { error, .. } if !error.is_empty() => {
                return Err(io::Error::other(format!("welcome error: {error}")))
            }
            Event::Welcome { panes, .. } => panes,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected welcome, got {other:?}"),
                ))
            }
        };

        let client = Arc::new(TermhostClient {
            writer: Mutex::new(writer),
            panes: Mutex::new(HashMap::new()),
            unclaimed_surviving: Mutex::new(surviving_panes.clone()),
            surviving_panes,
        });

        let weak = Arc::downgrade(&client);
        thread::Builder::new()
            .name("termhost-reader".into())
            .spawn(move || {
                while let Ok(ev) = proto::read_event(&mut reader) {
                    // Stop if the client has been dropped.
                    let Some(client) = weak.upgrade() else { break };
                    client.handle_event(ev);
                }
            })?;

        Ok(client)
    }

    /// Reconnects to the daemon and resumes event flow over the existing
    /// client, keeping every registered [`TermhostPane`] handle valid. Used
    /// when a live handoff fails after [`Self::detach_for_handoff`] already
    /// dropped the connection: the rolled-back server must reclaim the daemon
    /// or its panes go dark. Ends with a resync request per pane so frames
    /// and input modes replay.
    pub fn reattach(self: &Arc<Self>, path: &str) -> io::Result<()> {
        let stream = UnixStream::connect(path)?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;
        proto::write_command(
            &mut writer,
            &Command::Hello {
                protocol_version: proto::PROTOCOL_VERSION,
            },
        )?;
        match proto::read_event(&mut reader)? {
            Event::Welcome { error, .. } if !error.is_empty() => {
                return Err(io::Error::other(format!("welcome error: {error}")))
            }
            Event::Welcome { .. } => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected welcome, got {other:?}"),
                ))
            }
        }
        *self.writer.lock().unwrap() = writer;
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("termhost-reader".into())
            .spawn(move || {
                while let Ok(ev) = proto::read_event(&mut reader) {
                    let Some(client) = weak.upgrade() else { break };
                    client.handle_event(ev);
                }
            })?;
        let pane_ids: Vec<u32> = self.panes.lock().unwrap().keys().copied().collect();
        for pane_id in pane_ids {
            let _ = self.send(&Command::RequestResync { pane_id });
        }
        Ok(())
    }

    fn handle_event(&self, ev: Event) {
        match ev {
            Event::PaneFrame { pane_id, frame } => {
                if let Some(state) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    state.grid.lock().unwrap().apply(frame);
                }
            }
            Event::PaneCwd { pane_id, cwd } => {
                if let Some(state) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &state.sink {
                        sink(PaneSignal::Cwd(cwd));
                    }
                }
            }
            Event::PaneAgent {
                pane_id,
                agent,
                state,
                visible_blocker,
                visible_working,
            } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Agent {
                            agent,
                            state,
                            visible_blocker,
                            visible_working,
                        });
                    }
                }
            }
            Event::PaneClipboard { pane_id, data } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Clipboard(data));
                    }
                }
            }
            Event::PaneTitle { pane_id, title } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Title(title));
                    }
                }
            }
            Event::PaneSelection { pane_id, text } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    // Reply to a blocking extract_selection: hand it to the waiter.
                    // Take the slot so a late/duplicate reply has nowhere to go.
                    if let Some(tx) = pane.pending_selection.lock().unwrap().take() {
                        let _ = tx.send(text);
                    }
                }
            }
            Event::PaneText { pane_id, text } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(tx) = pane.pending_text.lock().unwrap().take() {
                        let _ = tx.send(text);
                    }
                }
            }
            Event::PaneModes {
                pane_id,
                alternate_screen,
                application_cursor,
                bracketed_paste,
                focus_reporting,
                mouse_mode,
                mouse_encoding,
                mouse_alternate_scroll,
                synchronized_output,
                kitty_keyboard_flags,
                modify_other_keys,
            } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Modes(PaneInputModes {
                            alternate_screen,
                            application_cursor,
                            bracketed_paste,
                            focus_reporting,
                            mouse_mode,
                            mouse_encoding,
                            mouse_alternate_scroll,
                            synchronized_output,
                            kitty_keyboard_flags,
                            modify_other_keys,
                        }));
                    }
                }
            }
            Event::PaneExited { pane_id, exit_code } => {
                if let Some(state) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    *state.exit.lock().unwrap() = Some(exit_code);
                }
            }
            Event::Error { pane_id, message } => {
                tracing::warn!(pane_id, message, "termhost backend error");
            }
            Event::Welcome { .. } => {}
        }
    }

    /// Pane IDs the daemon already had live when we connected (welcome.panes). A
    /// restarted/handed-off herdr reconciles its restored session against these:
    /// matching panes are adopted, not re-created.
    pub fn surviving_panes(&self) -> &[u32] {
        &self.surviving_panes
    }

    /// Atomically claims a surviving pane for adoption. Returns true exactly
    /// once per pane ID; later spawns with the same ID (shell respawn after
    /// the adopted process exited) create a fresh daemon pane instead.
    // Caller is the real spawn tail, cfg'd out of test builds.
    #[cfg_attr(test, allow(dead_code))]
    pub fn claim_surviving_pane(&self, pane_id: u32) -> bool {
        let mut unclaimed = self.unclaimed_surviving.lock().unwrap();
        match unclaimed.iter().position(|id| *id == pane_id) {
            Some(index) => {
                unclaimed.swap_remove(index);
                true
            }
            None => false,
        }
    }

    /// Closes any surviving daemon pane this herdr did NOT adopt or create during
    /// restore — a live shell the daemon kept (e.g. a pane spawned just before the
    /// previous herdr crashed, before its session was saved) that our restored
    /// session doesn't reference, and which would otherwise leak until the daemon's
    /// idle timeout. Call once, after restore. Returns how many were closed.
    pub fn close_orphans(&self) -> usize {
        let orphans: Vec<u32> = {
            let known = self.panes.lock().unwrap();
            self.surviving_panes
                .iter()
                .copied()
                .filter(|id| !known.contains_key(id))
                .collect()
        };
        let mut closed = 0;
        for id in orphans {
            if self.send(&Command::ClosePane { pane_id: id }).is_ok() {
                closed += 1;
            }
        }
        closed
    }

    /// Adopts a pane that already exists in the daemon (a survivor of a herdr
    /// restart/handoff): registers client-side state + signal sink WITHOUT sending
    /// CreatePane, then requests a resync so the pane repaints with its current
    /// state. The reverse of [`create_pane`] for the live-process case.
    pub fn adopt_pane(
        self: &Arc<Self>,
        pane_id: u32,
        sink: Option<SignalSink>,
    ) -> io::Result<TermhostPane> {
        let state = Arc::new(PaneState {
            grid: Mutex::new(PaneGrid::default()),
            exit: Mutex::new(None),
            sink,
            pending_selection: Mutex::new(None),
            pending_text: Mutex::new(None),
        });
        // Register before requesting the resync so the replayed events route here.
        self.panes.lock().unwrap().insert(pane_id, state.clone());
        self.send(&Command::RequestResync { pane_id })?;
        Ok(TermhostPane {
            client: self.clone(),
            id: pane_id,
            state,
        })
    }

    /// Spawns a pane on the backend and returns a handle to it. `sink` receives
    /// out-of-band pane signals (cwd, agent detection, …) on the reader thread.
    pub fn create_pane(
        self: &Arc<Self>,
        spec: PaneSpec,
        sink: Option<SignalSink>,
    ) -> io::Result<TermhostPane> {
        let state = Arc::new(PaneState {
            grid: Mutex::new(PaneGrid::default()),
            exit: Mutex::new(None),
            sink,
            pending_selection: Mutex::new(None),
            pending_text: Mutex::new(None),
        });
        self.panes
            .lock()
            .unwrap()
            .insert(spec.pane_id, state.clone());

        self.send(&Command::CreatePane {
            pane_id: spec.pane_id,
            cols: spec.cols,
            rows: spec.rows,
            cell_width_px: spec.cell_width_px,
            cell_height_px: spec.cell_height_px,
            cwd: spec.cwd,
            command: spec.command,
            args: spec.args,
            env: spec.env,
            initial_history: spec.initial_history,
        })?;

        Ok(TermhostPane {
            client: self.clone(),
            id: spec.pane_id,
            state,
        })
    }

    /// Tells a persistent daemon to exit and tear down its panes (clean herdr quit).
    /// Best-effort: if the daemon is already gone the write just fails and is ignored.
    pub fn request_shutdown(&self) {
        let _ = self.send(&Command::Shutdown);
    }

    /// Detaches from a persistent daemon for a live handoff WITHOUT closing panes.
    ///
    /// Shuts the socket down so the daemon reads EOF and returns from its serial
    /// `Attach`, freeing the single-writer slot for the replacement herdr (waiting in
    /// the accept backlog) to connect, resync, and adopt the live shells. The daemon
    /// keeps every pane running across the gap. After this, [`send`](Self::send) fails
    /// on the dead socket, so a later `close_pane` from a dropping [`TermhostPane`] is
    /// a harmless no-op — the shells survive for the replacement to adopt. Without this
    /// the handoff would deadlock: the old server can't exit (and free the slot) until
    /// the replacement is ready, but the replacement can't adopt until the slot frees.
    pub fn detach_for_handoff(&self) {
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }

    fn send(&self, cmd: &Command) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        proto::write_command(&mut *w, cmd)
    }
}

/// A handle to one pane on the Go backend. Implements [`TerminalBackend`].
pub struct TermhostPane {
    client: Arc<TermhostClient>,
    id: u32,
    state: Arc<PaneState>,
}

impl TermhostPane {
    /// Returns the current accumulated grid as a full frame, or `None` before
    /// the first frame arrives.
    pub fn snapshot(&self) -> Option<wire::FrameData> {
        self.state.grid.lock().unwrap().snapshot()
    }

    /// Returns whether the grid changed since the last call, clearing the flag.
    pub fn take_dirty(&self) -> bool {
        let mut grid = self.state.grid.lock().unwrap();
        std::mem::replace(&mut grid.dirty, false)
    }

    /// Returns the latest cursor state reported by the backend.
    pub fn cursor(&self) -> Option<wire::CursorState> {
        self.state.grid.lock().unwrap().cursor.clone()
    }

    /// Scrolls the pane's viewport by `delta` lines (negative = up into history,
    /// positive = toward the live bottom). The backend clamps and reports the new
    /// position on the next frame.
    pub fn scroll(&self, delta: i32) {
        let _ = self.client.send(&Command::ScrollViewport {
            pane_id: self.id,
            delta,
        });
    }

    /// Returns the latest scrollback position reported by the backend.
    pub fn scroll_metrics(&self) -> Option<proto::FrameScroll> {
        self.state.grid.lock().unwrap().scroll
    }

    /// Resolves the OSC 8 hyperlinks visible in the accumulated grid, within the
    /// `width`×`height` window at screen origin (`origin_x`, `origin_y`). Returns
    /// `((screen_x, screen_y), cell_symbol, uri)` per linked cell — the same shape
    /// the in-process emulator produces, so the native-TUI click resolver works for
    /// termhost panes (whose local emulator is unfed). The Go frame already carries
    /// the per-cell link index and the URI table.
    pub fn visible_hyperlinks(
        &self,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
    ) -> Vec<((u16, u16), String, String)> {
        self.state
            .grid
            .lock()
            .unwrap()
            .visible_hyperlinks(origin_x, origin_y, width, height)
    }

    /// Extracts the text of the selection bounded by the two screen-buffer
    /// endpoints, blocking until the Go backend (which owns the fed emulator)
    /// replies. The local emulator is unfed for termhost panes, so this round-trip
    /// is the only way to read selection text. Returns `None` on send failure or if
    /// no reply arrives within the timeout (the backend resolves and orders the
    /// coordinates and replies with a `pane_selection` event); an empty string means
    /// the range had no selectable content.
    pub fn extract_selection_blocking(
        &self,
        anchor_row: u32,
        anchor_col: u16,
        cursor_row: u32,
        cursor_col: u16,
        rectangle: bool,
    ) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        // Register the waiter before sending so the reply can't race ahead of us.
        *self.state.pending_selection.lock().unwrap() = Some(tx);

        if self
            .client
            .send(&Command::RequestSelection {
                pane_id: self.id,
                anchor: proto::SelectionPoint {
                    row: anchor_row,
                    col: anchor_col,
                },
                cursor: proto::SelectionPoint {
                    row: cursor_row,
                    col: cursor_col,
                },
                rectangle,
            })
            .is_err()
        {
            *self.state.pending_selection.lock().unwrap() = None;
            return None;
        }

        match rx.recv_timeout(SEAM_REPLY_TIMEOUT) {
            Ok(text) => Some(text),
            Err(_) => {
                // Timed out or the backend is gone: drop the stale waiter.
                *self.state.pending_selection.lock().unwrap() = None;
                None
            }
        }
    }

    /// Extracts buffer text from the backend, blocking until the pane_text reply.
    /// `scope` is [`proto::TEXT_SCOPE_VISIBLE`]/[`proto::TEXT_SCOPE_RECENT`]; `lines`
    /// bounds the recent scope (0 = whole buffer); `ansi`/`unwrap` select VT and
    /// soft-wrap rejoining. Returns `None` on send failure or timeout. The local
    /// emulator is unfed for termhost panes, so this round-trip is the only way to
    /// read their text.
    pub fn extract_text_blocking(
        &self,
        scope: u8,
        lines: u32,
        ansi: bool,
        unwrap: bool,
    ) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        *self.state.pending_text.lock().unwrap() = Some(tx);

        if self
            .client
            .send(&Command::RequestText {
                pane_id: self.id,
                scope,
                lines,
                ansi,
                unwrap,
            })
            .is_err()
        {
            *self.state.pending_text.lock().unwrap() = None;
            return None;
        }

        match rx.recv_timeout(SEAM_REPLY_TIMEOUT) {
            Ok(text) => Some(text),
            Err(_) => {
                *self.state.pending_text.lock().unwrap() = None;
                None
            }
        }
    }
}

/// Upper bound on a blocking request/response round-trip (selection, text). The
/// backend formats under its per-pane emulator lock, so a reply is normally
/// sub-millisecond over the local socket; this only guards against a wedged or dead
/// daemon hanging the UI thread.
const SEAM_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

impl TerminalBackend for TermhostPane {
    fn write_input(&self, bytes: &[u8]) {
        let _ = self.client.send(&Command::Input {
            pane_id: self.id,
            data: bytes.to_vec(),
        });
    }

    fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        let _ = self.client.send(&Command::Resize {
            pane_id: self.id,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        });
    }

    fn exit_status(&self) -> Option<i32> {
        *self.state.exit.lock().unwrap()
    }

    fn close(&self) {
        let _ = self.client.send(&Command::ClosePane { pane_id: self.id });
        self.client.panes.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn read_frame(r: &mut impl Read) -> Vec<u8> {
        let mut hdr = [0u8; 4];
        r.read_exact(&mut hdr).unwrap();
        let n = u32::from_le_bytes(hdr) as usize;
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).unwrap();
        buf
    }

    fn write_frame(w: &mut impl Write, json: &str) {
        w.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
        w.write_all(json.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    // A fake daemon over a real Unix socket validates the full blocking
    // request/response: connect handshake → create_pane → extract_selection_blocking
    // sends request_selection, the reader thread routes the pane_selection reply back
    // to the waiting caller.
    #[test]
    fn extract_selection_blocking_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("herdr-th-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let daemon = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _hello = read_frame(&mut conn); // Hello
            write_frame(&mut conn, r#"{"type":"welcome","protocol_version":1}"#);
            let _create = read_frame(&mut conn); // CreatePane
            let req = read_frame(&mut conn); // RequestSelection
                                             // Echo back proof the request reached us, then reply with the text.
            let req: serde_json::Value = serde_json::from_slice(&req).unwrap();
            assert_eq!(req["type"], "request_selection");
            assert_eq!(req["anchor"]["row"], 0);
            assert_eq!(req["cursor"]["col"], 4);
            write_frame(
                &mut conn,
                r#"{"type":"pane_selection","pane_id":1,"text":"HELLO"}"#,
            );
        });

        let client = TermhostClient::connect(path.to_str().unwrap()).unwrap();
        let pane = client
            .create_pane(
                PaneSpec {
                    pane_id: 1,
                    cols: 40,
                    rows: 5,
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        let text = pane.extract_selection_blocking(0, 0, 0, 4, false);
        assert_eq!(text, Some("HELLO".to_string()));

        daemon.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    // On reconnect the daemon reports its surviving panes in welcome.panes. After
    // restore adopts the ones the session references, close_orphans must close exactly
    // the rest (live shells the session no longer tracks) and leave adopted panes alone.
    #[test]
    fn close_orphans_closes_only_unadopted_survivors() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("herdr-th-orphan-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let daemon = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _hello = read_frame(&mut conn);
            // Reconnect: the daemon already has panes 1, 2, 3 live.
            write_frame(
                &mut conn,
                r#"{"type":"welcome","protocol_version":1,"panes":[1,2,3]}"#,
            );
            let _resync = read_frame(&mut conn); // request_resync for the adopted pane (2)
                                                 // close_orphans should now close 1 and 3 (not the adopted 2), in order.
            let mut closed = Vec::new();
            for _ in 0..2 {
                let cmd: serde_json::Value =
                    serde_json::from_slice(&read_frame(&mut conn)).unwrap();
                assert_eq!(cmd["type"], "close_pane");
                closed.push(cmd["pane_id"].as_u64().unwrap());
            }
            closed.sort_unstable();
            assert_eq!(closed, vec![1, 3]);
        });

        let client = TermhostClient::connect(path.to_str().unwrap()).unwrap();
        assert_eq!(client.surviving_panes(), &[1, 2, 3]);
        // Adopt only pane 2 (the one the restored session references).
        let _adopted = client.adopt_pane(2, None).unwrap();

        let closed = client.close_orphans();
        assert_eq!(closed, 2, "panes 1 and 3 are orphans");

        daemon.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_text_blocking_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("herdr-th-text-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let daemon = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _hello = read_frame(&mut conn);
            write_frame(&mut conn, r#"{"type":"welcome","protocol_version":1}"#);
            let _create = read_frame(&mut conn);
            let req = read_frame(&mut conn);
            let req: serde_json::Value = serde_json::from_slice(&req).unwrap();
            assert_eq!(req["type"], "request_text");
            assert_eq!(req["scope"], 1); // recent
            assert_eq!(req["unwrap"], true);
            assert!(req.get("lines").is_none()); // 0 omitted → whole buffer
            write_frame(
                &mut conn,
                r#"{"type":"pane_text","pane_id":1,"text":"row1\nrow2"}"#,
            );
        });

        let client = TermhostClient::connect(path.to_str().unwrap()).unwrap();
        let pane = client
            .create_pane(
                PaneSpec {
                    pane_id: 1,
                    cols: 40,
                    rows: 5,
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        let text = pane.extract_text_blocking(super::proto::TEXT_SCOPE_RECENT, 0, false, true);
        assert_eq!(text, Some("row1\nrow2".to_string()));

        daemon.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    fn cell(symbol: &str, hyperlink: Option<u32>) -> wire::CellData {
        wire::CellData {
            symbol: symbol.to_string(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink,
        }
    }

    #[test]
    fn grid_visible_hyperlinks_maps_cells_to_screen_and_uri() {
        // 3x2 grid: row 0 = "l k x" with l,k linked to URI 0; row 1 has no links.
        let mut grid = PaneGrid::default();
        grid.apply(proto::Frame {
            cols: 3,
            rows: 2,
            full: true,
            cursor: None,
            cells: vec![
                cell("l", Some(0)),
                cell("k", Some(0)),
                cell("x", None),
                cell(" ", None),
                cell(" ", None),
                cell(" ", None),
            ],
            hyperlinks: vec!["https://example.com".to_string()],
            scroll: None,
        });

        // Origin (5, 2): screen coords are offset by the pane's inner-rect origin.
        let links = grid.visible_hyperlinks(5, 2, 3, 2);
        assert_eq!(
            links,
            vec![
                ((5, 2), "l".to_string(), "https://example.com".to_string()),
                ((6, 2), "k".to_string(), "https://example.com".to_string()),
            ]
        );
    }

    #[test]
    fn grid_visible_hyperlinks_clips_to_window_and_empty_table() {
        let mut grid = PaneGrid::default();
        // A linked cell at column 2, but a 2-wide window excludes it.
        grid.apply(proto::Frame {
            cols: 3,
            rows: 1,
            full: true,
            cursor: None,
            cells: vec![cell("a", None), cell("b", None), cell("c", Some(0))],
            hyperlinks: vec!["https://x".to_string()],
            scroll: None,
        });
        assert!(grid.visible_hyperlinks(0, 0, 2, 1).is_empty());

        // No link table ⇒ nothing, even if a stale index were present.
        let mut bare = PaneGrid::default();
        bare.apply(proto::Frame {
            cols: 1,
            rows: 1,
            full: true,
            cursor: None,
            cells: vec![cell("a", None)],
            hyperlinks: vec![],
            scroll: None,
        });
        assert!(bare.visible_hyperlinks(0, 0, 1, 1).is_empty());
    }
}
