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

/// OSC-derived passthrough from a pane (working directory today; title/clipboard
/// can be added as the seam grows). Delivered to a per-pane [`OscSink`].
#[derive(Debug, Clone)]
pub enum PaneOsc {
    /// Working directory reported via OSC 7.
    Cwd(String),
}

/// Per-pane callback the owner installs to receive [`PaneOsc`] events. Invoked on
/// the client reader thread, so it must be cheap and non-blocking.
pub type OscSink = Box<dyn Fn(PaneOsc) + Send + Sync>;

/// Shared, reader-thread-updated state for one pane. The reader thread folds Go
/// frames into a full accumulated grid; the render path reads snapshots of it.
struct PaneState {
    grid: Mutex<PaneGrid>,
    exit: Mutex<Option<i32>>,
    /// Installed at creation; receives OSC passthrough events. Never mutated.
    osc: Option<OscSink>,
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
            hyperlinks: Vec::new(),
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
                    if let Some(sink) = &state.osc {
                        sink(PaneOsc::Cwd(cwd));
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

    /// Spawns a pane on the backend and returns a handle to it. `osc` receives
    /// OSC passthrough events (cwd, …) for this pane on the reader thread.
    pub fn create_pane(
        self: &Arc<Self>,
        spec: PaneSpec,
        osc: Option<OscSink>,
    ) -> io::Result<TermhostPane> {
        let state = Arc::new(PaneState {
            grid: Mutex::new(PaneGrid::default()),
            exit: Mutex::new(None),
            osc,
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
