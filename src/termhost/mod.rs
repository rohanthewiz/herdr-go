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

pub use client::{PaneInputModes, PaneSignal, PaneSpec, SignalSink, TermhostClient, TermhostPane};
pub use proto::{TEXT_SCOPE_RECENT, TEXT_SCOPE_VISIBLE};

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

/// After session restore, close any termhost pane the (reconnected) persistent daemon
/// still has that this herdr didn't adopt or create — a live shell our restored
/// session doesn't reference (drift from a prior crash), which would otherwise leak
/// until the daemon's idle timeout. Peeks the already-initialized client only: if the
/// backend was never used this run there's nothing to reconcile (and we don't force a
/// connect just to check).
pub(crate) fn close_restored_orphans() {
    if let Some(Some(client)) = CLIENT.get() {
        let closed = client.close_orphans();
        if closed > 0 {
            tracing::info!(closed, "closed orphaned termhost panes after restore");
        }
    }
}

/// Detaches the persistent termhost daemon connection for a live handoff, so the
/// replacement herdr can reconnect and adopt the live shells (see
/// [`client::TermhostClient::detach_for_handoff`]). Peeks the already-initialized
/// client only — if the backend was never used this run there's nothing to detach,
/// and we don't force a connect just to check.
#[cfg(unix)]
pub(crate) fn detach_for_handoff() {
    if let Some(Some(client)) = CLIENT.get() {
        client.detach_for_handoff();
        tracing::info!("detached termhost daemon for live handoff (panes kept alive)");
    }
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
    // Managed: reconnect to a surviving persistent daemon, or spawn a fresh one.
    if let Some(bin) = std::env::var(BIN_ENV_VAR).ok().filter(|p| !p.is_empty()) {
        return connect_or_spawn(&bin);
    }
    None
}

/// First tries to reconnect to a persistent daemon left running by a previous herdr
/// (a restart or binary handoff — its panes are still alive at the session socket);
/// only spawns a fresh daemon if none is reachable. The reconnect is what makes
/// termhost shells survive a herdr restart: the new herdr adopts the survivors
/// (reported in welcome.panes) instead of re-creating them.
fn connect_or_spawn(bin: &str) -> Option<Arc<TermhostClient>> {
    let socket = managed_socket_path();
    let socket_str = socket.to_string_lossy().into_owned();
    if let Ok(client) = TermhostClient::connect(&socket_str) {
        tracing::info!(socket = %socket_str, surviving = client.surviving_panes().len(),
            "reconnected to persistent termhost daemon");
        return Some(client);
    }
    spawn_and_connect(bin, socket)
}

/// Spawns the daemon binary in persistent mode, waits for it to listen, and
/// connects. On any failure it kills the child (if spawned) and falls back to
/// in-process. The socket is session-keyed (see [`managed_socket_path`]) so the
/// daemon is rediscoverable by a future herdr after a restart/handoff.
fn spawn_and_connect(bin: &str, socket: PathBuf) -> Option<Arc<TermhostClient>> {
    if let Some(parent) = socket.parent() {
        let _ = std::fs::create_dir_all(parent); // session data dir may not exist yet
    }
    let _ = std::fs::remove_file(&socket); // stale socket (connect above already failed)

    // Daemon logs would corrupt the TUI, so send them to a sibling log file (or
    // discard them if that can't be created).
    let log_path = socket.with_extension("log");
    let stdio = || match std::fs::File::create(&log_path) {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };

    let mut command = std::process::Command::new(bin);
    command
        .arg("--socket")
        .arg(&socket)
        .arg("--persistent")
        .stdout(stdio())
        .stderr(stdio());
    // Detach into its own session so the daemon outlives us: without setsid it shares
    // our controlling terminal and process group, and our death (and the closing tty)
    // would SIGHUP it — defeating persistence. setsid makes it a session leader with
    // no controlling terminal.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            // SAFETY: setsid in the forked child before exec; only async-signal-safe
            // libc calls. Failure (already a group leader — not the case here) is
            // non-fatal, so the error is intentionally ignored.
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match command.spawn() {
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

/// The persistent daemon's socket, keyed by the herdr *session* (not pid) so a
/// restarted or handed-off herdr rediscovers the same daemon — its live shells are
/// still running behind it. Lives in the session data dir alongside `herdr.sock`,
/// one daemon per session. Concurrent sessions key different dirs and don't collide.
fn managed_socket_path() -> PathBuf {
    crate::session::data_dir().join("herdr-termhost.sock")
}

/// Tears down the persistent daemon on a *clean* herdr quit: send `shutdown` so it
/// exits and removes its own socket, then reap a child we spawned (SIGTERM backstop).
/// A crash, panic, or binary handoff skips this — the connection just drops and the
/// daemon keeps its panes alive for the next herdr to reconnect and resync. (The
/// daemon's idle timeout is the backstop if no herdr ever comes back.)
pub fn shutdown() {
    // Tell the daemon to exit, whether we spawned it or merely reconnected to a
    // survivor — a clean quit means this session is done with it.
    if let Some(Some(client)) = CLIENT.get() {
        client.request_shutdown();
    }
    let Some(mut spawned) = SPAWNED.lock().unwrap().take() else {
        tracing::info!("termhost daemon sent shutdown (not supervised by us)");
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
