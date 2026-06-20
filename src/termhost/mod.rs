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

pub use client::{PaneSpec, TermhostClient, TermhostPane};

use crate::protocol as wire;

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
