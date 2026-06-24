//! Client for the Go `termhost` daemon: connects over a Unix socket, runs a
//! reader thread that fans incoming `pane_frame`/`pane_exited` events into
//! per-pane state, and hands out [`TermhostPane`] handles that implement
//! [`TerminalBackend`].

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

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
    /// Reply to a selection request: the extracted text (empty = no content). The
    /// owner copies it to the clipboard, the same way the in-process path does.
    Selection(String),
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
            &Command::Hello { protocol_version: proto::PROTOCOL_VERSION },
        )?;
        match proto::read_event(&mut reader)? {
            Event::Welcome { error, .. } if error.is_empty() => {}
            Event::Welcome { error, .. } => {
                return Err(io::Error::other(format!("welcome error: {error}")))
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected welcome, got {other:?}"),
                ))
            }
        }

        let client = Arc::new(TermhostClient {
            writer: Mutex::new(writer),
            panes: Mutex::new(HashMap::new()),
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
            Event::PaneAgent { pane_id, agent, state, visible_blocker, visible_working } => {
                if let Some(pane) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Agent { agent, state, visible_blocker, visible_working });
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
                    if let Some(sink) = &pane.sink {
                        sink(PaneSignal::Selection(text));
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
        });
        self.panes.lock().unwrap().insert(spec.pane_id, state.clone());

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
        })?;

        Ok(TermhostPane { client: self.clone(), id: spec.pane_id, state })
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
        let _ = self.client.send(&Command::ScrollViewport { pane_id: self.id, delta });
    }

    /// Returns the latest scrollback position reported by the backend.
    pub fn scroll_metrics(&self) -> Option<proto::FrameScroll> {
        self.state.grid.lock().unwrap().scroll
    }

    /// Requests the text of the selection bounded by the two screen-buffer
    /// endpoints. The Go backend resolves and orders the coordinates and replies
    /// asynchronously with a `pane_selection` event, delivered as
    /// [`PaneSignal::Selection`] to this pane's sink.
    pub fn request_selection(
        &self,
        anchor_row: u32,
        anchor_col: u16,
        cursor_row: u32,
        cursor_col: u16,
        rectangle: bool,
    ) {
        let _ = self.client.send(&Command::RequestSelection {
            pane_id: self.id,
            anchor: proto::SelectionPoint { row: anchor_row, col: anchor_col },
            cursor: proto::SelectionPoint { row: cursor_row, col: cursor_col },
            rectangle,
        });
    }
}

impl TerminalBackend for TermhostPane {
    fn write_input(&self, bytes: &[u8]) {
        let _ = self.client.send(&Command::Input { pane_id: self.id, data: bytes.to_vec() });
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

    fn latest_frame(&self) -> Option<wire::FrameData> {
        self.snapshot()
    }

    fn exit_status(&self) -> Option<i32> {
        *self.state.exit.lock().unwrap()
    }

    fn close(&self) {
        let _ = self.client.send(&Command::ClosePane { pane_id: self.id });
        self.client.panes.lock().unwrap().remove(&self.id);
    }
}
