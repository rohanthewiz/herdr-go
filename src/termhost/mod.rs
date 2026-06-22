//! Phase B: the Go↔Rust orchestration seam from the Rust (orchestrator) side.
//!
//! Behind the `termhost` Cargo feature. This module is scaffolding: it defines
//! the [`TerminalBackend`] abstraction and a client for the Go `termhost`
//! daemon, but does NOT yet rewire [`crate::pane::PaneRuntime`] — that happens in
//! the next step. With the feature off (the default), nothing here compiles, so
//! the existing in-process PTY + ghostty path is completely unaffected.
//!
//! See ai_docs/phase-b-orchestration-seam.md (in the herdr-web repo) for the
//! protocol design.
#![allow(dead_code, unused_imports)] // wired into PaneRuntime in the next integration step

mod client;
mod proto;

pub use client::{OscSink, PaneOsc, PaneSpec, TermhostClient, TermhostPane};

use crate::protocol as wire;
use std::sync::{Arc, OnceLock};

/// Env var naming the Go `termhost` daemon's Unix socket. When set (and the
/// `termhost` feature is compiled in), panes spawn on the Go backend instead of
/// the in-process PTY + ghostty path. Unset → the default in-process path.
pub const SOCKET_ENV_VAR: &str = "HERDR_TERMHOST_SOCKET";

/// The process-wide connection to the Go backend, established lazily on first
/// use. `None` means the backend is disabled (env unset) or the connection
/// failed (in which case we log and fall back to the in-process path).
static CLIENT: OnceLock<Option<Arc<TermhostClient>>> = OnceLock::new();

/// Returns the shared termhost client if the backend is enabled and reachable,
/// connecting on first call. Cached for the life of the process.
pub(crate) fn client_if_enabled() -> Option<Arc<TermhostClient>> {
    CLIENT
        .get_or_init(|| {
            let path = std::env::var(SOCKET_ENV_VAR).ok().filter(|p| !p.is_empty())?;
            match TermhostClient::connect(&path) {
                Ok(client) => {
                    tracing::info!(path, "connected to Go termhost terminal backend");
                    Some(client)
                }
                Err(err) => {
                    tracing::error!(
                        path,
                        error = %err,
                        "failed to connect to termhost backend; falling back to in-process PTY"
                    );
                    None
                }
            }
        })
        .clone()
}

/// A pane's terminal runtime, abstracted over where the PTY + VT emulation live.
///
/// The existing in-process portable-pty + ghostty path and the Go `termhost`
/// backend both fit this shape: feed user input, resize, pull the latest
/// rendered grid, observe exit, and tear down. PaneRuntime will be expressed in
/// terms of this trait so either backend can drive a pane.
pub trait TerminalBackend: Send + Sync {
    /// Writes raw user input (already encoded) to the pane's PTY.
    fn write_input(&self, bytes: &[u8]);

    /// Resizes the pane's PTY and emulator.
    fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32);

    /// Returns the latest rendered grid for the pane, if one is available.
    fn latest_frame(&self) -> Option<wire::FrameData>;

    /// Returns the child process exit code once it has exited.
    fn exit_status(&self) -> Option<i32>;

    /// Tears the pane down and releases its resources.
    fn close(&self);
}
