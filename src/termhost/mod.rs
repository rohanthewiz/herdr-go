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

pub use client::{PaneSignal, PaneSpec, SignalSink, TermhostClient, TermhostPane};

use crate::protocol as wire;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Env var naming an already-running Go `termhost` daemon's Unix socket. When set
/// (and the `termhost` feature is compiled in), panes use that backend instead of
/// the in-process PTY + ghostty path. This is the dev/manual path — the daemon is
/// hand-launched and outlives the orchestrator.
pub const SOCKET_ENV_VAR: &str = "HERDR_TERMHOST_SOCKET";

/// Env var naming the Go `termhost` daemon *binary*. When set (and `SOCKET_ENV_VAR`
/// is not), the orchestrator spawns and supervises the daemon itself: it picks a
/// socket, launches the binary in managed mode, connects, and tears it down on
/// shutdown. This is the normal path — no hand-launch required.
pub const BIN_ENV_VAR: &str = "HERDR_TERMHOST_BIN";

/// How long to wait for a freshly spawned daemon to start listening.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// The process-wide connection to the Go backend, established lazily on first
/// use. `None` means the backend is disabled (neither env set) or it could not be
/// reached/spawned (in which case we log and fall back to the in-process path).
static CLIENT: OnceLock<Option<Arc<TermhostClient>>> = OnceLock::new();

/// A daemon this process spawned and is responsible for tearing down. Empty when
/// connecting to a hand-launched daemon (`SOCKET_ENV_VAR`) or when disabled.
static SPAWNED: Mutex<Option<SpawnedDaemon>> = Mutex::new(None);

struct SpawnedDaemon {
    child: Child,
    socket: PathBuf,
}

/// Returns the shared termhost client if the backend is enabled and reachable,
/// connecting (and spawning the daemon if managed) on first call. Cached for the
/// life of the process.
pub(crate) fn client_if_enabled() -> Option<Arc<TermhostClient>> {
    CLIENT.get_or_init(connect_backend).clone()
}

fn connect_backend() -> Option<Arc<TermhostClient>> {
    // Dev/manual: attach to a hand-launched daemon at a known socket.
    if let Some(path) = std::env::var(SOCKET_ENV_VAR).ok().filter(|p| !p.is_empty()) {
        return match TermhostClient::connect(&path) {
            Ok(client) => {
                tracing::info!(path, "connected to hand-launched termhost backend");
                Some(client)
            }
            Err(err) => {
                tracing::error!(path, error = %err,
                    "failed to connect to termhost backend; falling back to in-process PTY");
                None
            }
        };
    }
    // Managed: spawn and supervise the daemon binary ourselves.
    if let Some(bin) = std::env::var(BIN_ENV_VAR).ok().filter(|p| !p.is_empty()) {
        return spawn_and_connect(&bin);
    }
    None
}

/// Spawns the daemon binary in managed mode, waits for it to listen, and connects.
/// On any failure it kills the child (if spawned) and falls back to in-process.
fn spawn_and_connect(bin: &str) -> Option<Arc<TermhostClient>> {
    let socket = managed_socket_path();
    let _ = std::fs::remove_file(&socket); // clear a stale socket from a prior crash

    // Daemon logs would corrupt the TUI, so send them to a sibling log file (or
    // discard them if that can't be created).
    let log_path = socket.with_extension("log");
    let stdio = || match std::fs::File::create(&log_path) {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };

    let mut child = match std::process::Command::new(bin)
        .arg("--socket")
        .arg(&socket)
        .arg("--exit-on-disconnect")
        .stdout(stdio())
        .stderr(stdio())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::error!(bin, error = %err,
                "failed to spawn termhost daemon; falling back to in-process PTY");
            return None;
        }
    };

    // Retry connect until the daemon is listening. Attempts before it binds fail
    // with connection-refused (the daemon never accepts them, so they don't count
    // as the client) — the first *successful* connect is the one we keep.
    let socket_str = socket.to_string_lossy().into_owned();
    let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            tracing::error!(bin, %status, "termhost daemon exited before listening; see {log_path:?}");
            return None;
        }
        match TermhostClient::connect(&socket_str) {
            Ok(client) => {
                tracing::info!(socket = %socket_str, bin, "spawned and connected to termhost backend");
                *SPAWNED.lock().unwrap() = Some(SpawnedDaemon { child, socket });
                return Some(client);
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                tracing::error!(socket = %socket_str, error = %err,
                    "termhost daemon never became reachable; killing it and falling back to in-process PTY");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A short, per-process socket path. Kept short (sockaddr_un.sun_path is ~104
/// bytes on macOS) and disambiguated by pid so concurrent herdr instances don't
/// collide.
fn managed_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("herdr-termhost-{}.sock", std::process::id()))
}

/// Tears down a daemon this process spawned: SIGTERM (so it cleans up its socket)
/// then reap. A no-op when attached to a hand-launched daemon or disabled. Called
/// on orchestrator shutdown; the daemon's `--exit-on-disconnect` is the backstop
/// if we exit without calling this (e.g. a panic).
pub fn shutdown() {
    let Some(mut spawned) = SPAWNED.lock().unwrap().take() else {
        return;
    };
    #[cfg(unix)]
    // SAFETY: child.id() is this process's direct child; SIGTERM is always valid.
    unsafe {
        libc::kill(spawned.child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = spawned.child.kill();
    let _ = spawned.child.wait();
    let _ = std::fs::remove_file(&spawned.socket);
    tracing::info!("termhost daemon shut down");
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
