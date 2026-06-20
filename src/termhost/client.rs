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

/// Shared, reader-thread-updated state for one pane.
struct PaneState {
    latest: Mutex<Option<wire::FrameData>>,
    exit: Mutex<Option<i32>>,
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
                return Err(io::Error::new(io::ErrorKind::Other, format!("welcome error: {error}")))
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
            .spawn(move || loop {
                let ev = match proto::read_event(&mut reader) {
                    Ok(ev) => ev,
                    Err(_) => break, // connection closed
                };
                // Stop if the client has been dropped.
                let Some(client) = weak.upgrade() else { break };
                client.handle_event(ev);
            })?;

        Ok(client)
    }

    fn handle_event(&self, ev: Event) {
        match ev {
            Event::PaneFrame { pane_id, frame } => {
                if let Some(state) = self.panes.lock().unwrap().get(&pane_id).cloned() {
                    *state.latest.lock().unwrap() = Some(frame.into_frame_data());
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

    /// Spawns a pane on the backend and returns a handle to it.
    pub fn create_pane(self: &Arc<Self>, spec: PaneSpec) -> io::Result<TermhostPane> {
        let state = Arc::new(PaneState {
            latest: Mutex::new(None),
            exit: Mutex::new(None),
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
        self.state.latest.lock().unwrap().clone()
    }

    fn exit_status(&self) -> Option<i32> {
        *self.state.exit.lock().unwrap()
    }

    fn close(&self) {
        let _ = self.client.send(&Command::ClosePane { pane_id: self.id });
        self.client.panes.lock().unwrap().remove(&self.id);
    }
}
