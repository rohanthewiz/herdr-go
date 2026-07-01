//! End-to-end smoke test for the Phase B termhost backend (step 3).
//!
//! Gated on `--features termhost`. Requires a running Go `termhost` daemon whose
//! Unix socket is named by the `HERDR_TERMHOST_SOCKET` env var in this test's
//! environment; if that is unset or unreachable, the test skips (passes) so it
//! is safe to run unconditionally.
//!
//! It spawns a real `herdr server` with the backend enabled, creates a workspace
//! over the JSON-RPC API (whose root pane is therefore spawned on the Go daemon),
//! attaches as a client, drives `echo <marker>` into the pane, and asserts the
//! marker shows up in a rendered frame. The frame's cells originate from the Go
//! emulator, so this exercises create_pane -> input -> frame across the seam and
//! the step-3 render path. `pane.read` for a termhost pane is served from the Go
//! backend over the seam (request_text), so it returns the program's output too.
#![cfg(feature = "termhost")]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;
use serde_json::{json, Value};
use support::{
    cleanup_test_base, client_handshake, read_server_message, register_runtime_dir,
    register_spawned_herdr_pid, unregister_spawned_herdr_pid, wait_for_file, wait_for_socket,
};

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/herdr-termhost-e2e-{}-{nanos}", std::process::id()))
}

struct SpawnedHerdr {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        drop(self._master.take());
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn spawn_server(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    termhost_socket: &str,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(config_home.join("herdr/config.toml"), "onboarding = false\n").unwrap();

    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    // Enable the Go terminal backend for this server.
    cmd.env("HERDR_TERMHOST_SOCKET", termhost_socket);

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr { _master: Some(pair.master), child }
}

/// Like [`spawn_server`] but in *managed* mode: instead of attaching to a
/// hand-launched daemon, herdr is told the daemon *binary* (HERDR_TERMHOST_BIN) and
/// spawns/supervises it itself. `tmpdir` becomes the child's TMPDIR so the managed
/// socket (`herdr-termhost-<pid>.sock`) lands in a path the test can locate.
fn spawn_server_managed(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    daemon_bin: &str,
    tmpdir: &PathBuf,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::create_dir_all(tmpdir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(config_home.join("herdr/config.toml"), "onboarding = false\n").unwrap();

    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("TMPDIR", tmpdir); // controls std::env::temp_dir() → managed socket location
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    // Managed mode: give herdr the daemon binary, not a pre-existing socket.
    cmd.env_remove("HERDR_TERMHOST_SOCKET");
    cmd.env("HERDR_TERMHOST_BIN", daemon_bin);

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr { _master: Some(pair.master), child }
}

/// The persistent daemon's session-keyed socket: `data_dir()/herdr-termhost.sock`.
/// In a debug test build `app_dir_name()` is `herdr-dev`; with no `HERDR_SESSION`
/// the data dir is just the config dir.
fn termhost_socket_path(config_home: &Path, session: Option<&str>) -> PathBuf {
    let app_dir = if cfg!(debug_assertions) { "herdr-dev" } else { "herdr" };
    let mut dir = config_home.join(app_dir);
    if let Some(name) = session {
        dir = dir.join("sessions").join(name);
    }
    dir.join("herdr-termhost.sock")
}

/// Cleanly stops a *persistent* daemon by speaking the framed protocol directly:
/// connect, say hello, send `shutdown`. Used to reap the daemon a test left running
/// (it deliberately outlives the herdr that spawned it). Also exercises the real
/// shutdown command over a socket. No-op if nothing is listening.
fn termhost_send_shutdown(socket: &Path) {
    let Ok(mut stream) = UnixStream::connect(socket) else { return };
    for msg in [
        r#"{"type":"hello","protocol_version":1}"#,
        r#"{"type":"shutdown"}"#,
    ] {
        let bytes = msg.as_bytes();
        if stream.write_all(&(bytes.len() as u32).to_le_bytes()).is_err()
            || stream.write_all(bytes).is_err()
        {
            return;
        }
    }
    let _ = stream.flush();
    thread::sleep(Duration::from_millis(150)); // let the daemon process the shutdown
}

/// Waits until `path` exists (or the timeout elapses), returning whether it does.
fn wait_until_exists(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

/// Like [`spawn_server_managed`] but bound to a *named* session (HERDR_SESSION), so
/// herdr persists/restores the session across a restart and all its sockets live in
/// the session data dir. Used to prove termhost shells survive a herdr restart.
fn spawn_server_managed_session(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    daemon_bin: &str,
    tmpdir: &PathBuf,
    session: &str,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(config_home.join("herdr-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::create_dir_all(tmpdir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(config_home.join("herdr/config.toml"), "onboarding = false\n").unwrap();
    fs::write(config_home.join("herdr-dev/config.toml"), "onboarding = false\n").unwrap();

    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("TMPDIR", tmpdir);
    cmd.env("HERDR_SESSION", session); // session-scoped sockets + persisted session file
    cmd.env_remove("HERDR_SOCKET_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    cmd.env_remove("HERDR_TERMHOST_SOCKET");
    cmd.env("HERDR_TERMHOST_BIN", daemon_bin);

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr { _master: Some(pair.master), child }
}

/// Polls `pane.read` (served from the Go buffer over the seam) until the pane's
/// recent text contains `needle`, or the timeout elapses.
fn wait_for_pane_text(api: &Path, pane_id: &str, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let read = send_json_request(
            api,
            "r",
            "pane.read",
            json!({ "pane_id": pane_id, "source": "recent", "lines": 200 }),
        );
        if let Some(text) = read["result"]["read"]["text"].as_str() {
            if text.contains(needle) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(150));
    }
    false
}

fn send_json_request(socket_path: &Path, id: &str, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    let request = json!({ "id": id, "method": method, "params": params });
    writeln!(stream, "{request}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("response should be valid JSON")
}

/// Like [`send_json_request`] but tolerant: returns `None` on any connect/IO/parse
/// error instead of panicking. Used while the API socket is mid-rebind (e.g. during
/// a live handoff, when the old server removes the socket and the replacement is
/// still binding it).
fn try_send_json_request(socket_path: &Path, id: &str, method: &str, params: Value) -> Option<Value> {
    let mut stream = UnixStream::connect(socket_path).ok()?;
    let request = json!({ "id": id, "method": method, "params": params });
    writeln!(stream, "{request}").ok()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).ok()?;
    serde_json::from_str(&response).ok()
}

/// Waits for the JSON-RPC API at `socket_path` to answer a `ping` with a result.
/// Tolerates the socket being absent/unbound (returns false on timeout) so it is
/// safe to call across a handoff where the replacement server is still coming up.
fn wait_for_api_ready(socket_path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(resp) = try_send_json_request(socket_path, "ping", "ping", json!({})) {
            if resp.get("result").is_some() {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct FrameWire {
    cells: Vec<CellWire>,
    width: u16,
    height: u16,
    cursor: Option<CursorWire>,
    hyperlinks: Vec<String>,
    graphics: Vec<u8>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CellWire {
    symbol: String,
    fg: u32,
    bg: u32,
    modifier: u16,
    skip: bool,
    hyperlink: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CursorWire {
    x: u16,
    y: u16,
    visible: bool,
    shape: u8,
}

fn decode_frame_payload(payload: &[u8]) -> Option<FrameWire> {
    bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .ok()
        .map(|(frame, _consumed): (FrameWire, usize)| frame)
}

fn frame_text(frame: &FrameWire) -> String {
    let width = frame.width.max(1) as usize;
    let mut text = String::new();
    for row in frame.cells.chunks(width) {
        for cell in row {
            text.push_str(&cell.symbol);
        }
        text.push('\n');
    }
    text
}

/// Reads frames until one renders `needle`, or the timeout elapses.
fn wait_for_rendered_text(stream: &mut UnixStream, needle: &str, timeout: Duration) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((1, payload)) => {
                if let Some(frame) = decode_frame_payload(&payload) {
                    if frame_text(&frame).contains(needle) {
                        return true;
                    }
                }
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    false
}

#[test]
fn termhost_pane_renders_shell_output_to_client() {
    let _lock = test_lock();

    // Skip gracefully if no daemon is wired up for this run.
    let termhost_socket = match std::env::var("HERDR_TERMHOST_SOCKET") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            eprintln!(
                "SKIP termhost_e2e: set HERDR_TERMHOST_SOCKET to a running Go termhost daemon socket"
            );
            return;
        }
    };
    if UnixStream::connect(&termhost_socket).is_err() {
        eprintln!("SKIP termhost_e2e: no daemon reachable at {termhost_socket}");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &termhost_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Create a workspace; its root pane spawns on the Go termhost backend.
    let create = send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "e2e" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");
    let workspace_id = create["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create should return workspace.workspace_id: {create}"))
        .to_string();
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create should return root_pane.pane_id: {create}"))
        .to_string();

    // The server auto-creates a default workspace, so the one we just created is NOT
    // the focused/displayed one. A client renders the *focused* workspace, so focus
    // ours before driving input or its frames won't reach the client.
    let focus = send_json_request(
        &api_socket,
        "focus",
        "workspace.focus",
        json!({ "workspace_id": workspace_id }),
    );
    assert!(focus.get("error").is_none(), "workspace.focus failed: {focus}");

    // Attach a client to receive rendered frames for the active workspace.
    let mut stream =
        UnixStream::connect(&client_socket).expect("should connect to client socket");
    let (version, error) =
        client_handshake(&mut stream, 13, 80, 24).expect("handshake should succeed");
    assert_eq!(version, 13, "server should report protocol version 13");
    assert!(error.is_none(), "handshake error: {error:?}");

    // Drive a command into the termhost-backed pane.
    let marker = "herdr_e2e_marker_42";
    let resp = send_json_request(
        &api_socket,
        "send",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {marker}\n") }),
    );
    assert!(resp.get("error").is_none(), "pane.send_text failed: {resp}");

    let saw_marker = wait_for_rendered_text(&mut stream, marker, Duration::from_secs(15));
    assert!(
        saw_marker,
        "termhost-backed pane should render '{marker}' (typed echo and/or its output) in a frame"
    );

    // Text extraction parity: pane.read for a termhost pane goes through the seam
    // (request_text → the Go backend's buffer), since the local Rust emulator is
    // unfed. So the marker the program printed must come back here too — proving the
    // Go side served the read, not a (degraded) empty local emulator.
    let read = send_json_request(
        &api_socket,
        "read",
        "pane.read",
        json!({ "pane_id": pane_id, "source": "recent", "lines": 200 }),
    );
    let read_text = read["result"]["read"]["text"].as_str().unwrap_or_default();
    eprintln!("termhost_e2e: pane.read (via seam) len={}", read_text.len());
    assert!(
        read_text.contains(marker),
        "pane.read should return the Go backend's buffer for a termhost pane, but '{marker}' was missing; got {read_text:?}"
    );

    // OSC 7 passthrough: make the shell emit a working-directory report and assert
    // it reaches the Rust pane (pane.get cwd). The Go Host scans the raw stream for
    // OSC 7 and sends a pane_cwd event; the client routes it to reported_cwd.
    let osc7 = r"printf '\033]7;file://localhost/tmp\033\\'";
    let resp = send_json_request(
        &api_socket,
        "osc",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("{osc7}\n") }),
    );
    assert!(resp.get("error").is_none(), "pane.send_text (osc7) failed: {resp}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_cwd = String::new();
    while Instant::now() < deadline {
        let info = send_json_request(&api_socket, "get", "pane.get", json!({ "pane_id": pane_id }));
        if let Some(cwd) = info["result"]["pane"]["cwd"].as_str() {
            got_cwd = cwd.to_string();
            if cwd == "/tmp" {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        got_cwd, "/tmp",
        "OSC 7 cwd should propagate to the termhost pane (pane.get cwd), got {got_cwd:?}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn termhost_pane_survives_client_reattach() {
    let _lock = test_lock();

    let termhost_socket = match std::env::var("HERDR_TERMHOST_SOCKET") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            eprintln!("SKIP termhost reattach: HERDR_TERMHOST_SOCKET unset");
            return;
        }
    };
    if UnixStream::connect(&termhost_socket).is_err() {
        eprintln!("SKIP termhost reattach: no daemon reachable at {termhost_socket}");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &termhost_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    let create =
        send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "reattach" }));
    let workspace_id = create["result"]["workspace"]["workspace_id"].as_str().unwrap().to_string();
    let pane_id = create["result"]["root_pane"]["pane_id"].as_str().unwrap().to_string();
    let focus = send_json_request(
        &api_socket,
        "focus",
        "workspace.focus",
        json!({ "workspace_id": workspace_id }),
    );
    assert!(focus.get("error").is_none(), "workspace.focus failed: {focus}");

    // Client 1: drive a first marker, then detach (drop the socket).
    {
        let mut c1 = UnixStream::connect(&client_socket).expect("connect client 1");
        client_handshake(&mut c1, 13, 80, 24).expect("handshake 1");
        let m1 = "reattach_before_42";
        send_json_request(
            &api_socket,
            "s1",
            "pane.send_text",
            json!({ "pane_id": pane_id, "text": format!("echo {m1}\n") }),
        );
        assert!(
            wait_for_rendered_text(&mut c1, m1, Duration::from_secs(15)),
            "client 1 should see '{m1}'"
        );
    } // c1 dropped — the TUI client detaches; the server (and its daemon) keep running.

    // Client 2 reattaches: the same pane/shell must still be alive — its scrollback
    // still holds the first marker, and a new command runs in the same shell.
    let mut c2 = UnixStream::connect(&client_socket).expect("connect client 2");
    client_handshake(&mut c2, 13, 80, 24).expect("handshake 2");
    let m2 = "reattach_after_99";
    send_json_request(
        &api_socket,
        "s2",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {m2}\n") }),
    );
    assert!(
        wait_for_rendered_text(&mut c2, m2, Duration::from_secs(15)),
        "reattached client should see '{m2}' — the termhost pane/shell survived client detach"
    );
    // The pane's buffer still holds the pre-detach output (read over the seam).
    let read = send_json_request(
        &api_socket,
        "read",
        "pane.read",
        json!({ "pane_id": pane_id, "source": "recent", "lines": 200 }),
    );
    let read_text = read["result"]["read"]["text"].as_str().unwrap_or_default();
    assert!(
        read_text.contains("reattach_before_42"),
        "pre-detach output should survive in the same pane; got {read_text:?}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn termhost_managed_daemon_is_persistent_and_survives_herdr_death() {
    let _lock = test_lock();

    // Skip unless told where the Go termhost binary is.
    let daemon_bin = match std::env::var("HERDR_TERMHOST_BIN") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP termhost managed: set HERDR_TERMHOST_BIN to the built Go termhost binary");
            return;
        }
    };
    if !Path::new(&daemon_bin).exists() {
        eprintln!("SKIP termhost managed: HERDR_TERMHOST_BIN {daemon_bin} not found");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let tmpdir = base.join("tmp");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned =
        spawn_server_managed(&config_home, &runtime_dir, &api_socket, &daemon_bin, &tmpdir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Create a workspace → herdr lazily spawns the managed daemon for the root pane.
    let create =
        send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "managed" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");

    // The persistent daemon binds the session-keyed socket once herdr spawns it for
    // the pane — proof the orchestrator launched and connected to a daemon it manages.
    let managed_socket = termhost_socket_path(&config_home, None);
    assert!(
        wait_until_exists(&managed_socket, Duration::from_secs(10)),
        "herdr should have spawned a persistent daemon at {managed_socket:?} after pane creation"
    );

    // Persistence contract: kill herdr (no graceful shutdown). The daemon must
    // OUTLIVE it — detached via setsid and ignoring the SIGHUP from the closing
    // controlling terminal — so a future herdr can reconnect to its live shells.
    drop(spawned); // SIGKILLs herdr
    thread::sleep(Duration::from_secs(1)); // long enough for any stray SIGHUP to land
    assert!(
        managed_socket.exists() && UnixStream::connect(&managed_socket).is_ok(),
        "persistent daemon should survive the orchestrator's death and stay reachable at {managed_socket:?}"
    );

    // Clean up the surviving daemon (it deliberately outlived herdr) via shutdown,
    // which also confirms the shutdown command makes it exit and remove its socket.
    termhost_send_shutdown(&managed_socket);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && managed_socket.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !managed_socket.exists(),
        "daemon should exit and remove its socket after a shutdown command, but {managed_socket:?} remains"
    );
    cleanup_test_base(&base);
}

/// The clean-quit counterpart to the persistence test: when herdr exits *cleanly*
/// (SIGINT, not a crash/handoff), it tells the persistent daemon to shut down so it
/// doesn't linger to its idle timeout.
#[test]
fn termhost_clean_server_quit_stops_daemon() {
    let _lock = test_lock();

    let daemon_bin = match std::env::var("HERDR_TERMHOST_BIN") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP termhost clean-quit: set HERDR_TERMHOST_BIN to the built Go termhost binary");
            return;
        }
    };
    if !Path::new(&daemon_bin).exists() {
        eprintln!("SKIP termhost clean-quit: HERDR_TERMHOST_BIN {daemon_bin} not found");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let tmpdir = base.join("tmp");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned =
        spawn_server_managed(&config_home, &runtime_dir, &api_socket, &daemon_bin, &tmpdir);
    let herdr_pid = spawned.child.process_id().expect("herdr should report a pid");
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));
    send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "clean" }));
    let managed_socket = termhost_socket_path(&config_home, None);
    assert!(
        wait_until_exists(&managed_socket, Duration::from_secs(10)),
        "herdr should have spawned a persistent daemon at {managed_socket:?}"
    );

    // Clean quit (SIGINT) → herdr sends the daemon a shutdown command on exit.
    // SAFETY: herdr_pid is a process we spawned; SIGINT is always valid.
    unsafe {
        libc::kill(herdr_pid as libc::pid_t, libc::SIGINT);
    }

    // The daemon should exit and remove its socket promptly — not linger to its idle
    // timeout — proving the clean-quit path tore it down.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && managed_socket.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !managed_socket.exists(),
        "a clean server quit should stop the persistent daemon, but {managed_socket:?} remains"
    );

    drop(spawned); // herdr is already exiting; ensure it's reaped
    cleanup_test_base(&base);
}

/// The headline 3b proof: a termhost shell SURVIVES a full herdr restart. herdr A
/// spawns a persistent daemon and a pane; herdr A is killed (daemon + shell live
/// on); herdr B restarts the same session, reconnects to the daemon, and ADOPTS the
/// surviving shell — its pre-restart output is still there and it runs new commands.
#[test]
fn termhost_pane_survives_herdr_restart() {
    let _lock = test_lock();

    let daemon_bin = match std::env::var("HERDR_TERMHOST_BIN") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP termhost restart: set HERDR_TERMHOST_BIN to the built Go termhost binary");
            return;
        }
    };
    if !Path::new(&daemon_bin).exists() {
        eprintln!("SKIP termhost restart: HERDR_TERMHOST_BIN {daemon_bin} not found");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let tmpdir = base.join("tmp");
    let session = "persist";
    let app_dir = if cfg!(debug_assertions) { "herdr-dev" } else { "herdr" };
    let session_dir = config_home.join(app_dir).join("sessions").join(session);
    let api_socket = session_dir.join("herdr.sock");
    let termhost_socket = session_dir.join("herdr-termhost.sock");
    let session_file = session_dir.join("session.json");

    // --- herdr A: create a pane, run a marker, let the session save ---
    let herdr_a = spawn_server_managed_session(&config_home, &runtime_dir, &daemon_bin, &tmpdir, session);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let create =
        send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "persist" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no root_pane.pane_id: {create}"))
        .to_string();
    assert!(
        wait_until_exists(&termhost_socket, Duration::from_secs(10)),
        "persistent daemon socket should appear at {termhost_socket:?}"
    );

    let m1 = "survive_marker_111";
    send_json_request(
        &api_socket,
        "s1",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {m1}\n") }),
    );
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m1, Duration::from_secs(15)),
        "herdr A's pane should show {m1}"
    );
    assert!(
        wait_until_exists(&session_file, Duration::from_secs(20)),
        "session should be persisted to {session_file:?} so herdr B can restore it"
    );

    // --- kill herdr A; the daemon and the live shell must persist ---
    drop(herdr_a); // SIGKILL — no graceful path runs
    thread::sleep(Duration::from_secs(1));
    assert!(
        UnixStream::connect(&termhost_socket).is_ok(),
        "the persistent daemon (and its shell) should outlive herdr A"
    );

    // --- herdr B: restore the session, reconnect to the daemon, adopt the shell ---
    let herdr_b = spawn_server_managed_session(&config_home, &runtime_dir, &daemon_bin, &tmpdir, session);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // The pre-restart output is still in the SAME shell's buffer — the daemon kept
    // the live process and herdr B adopted it (rather than re-spawning a fresh shell).
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m1, Duration::from_secs(20)),
        "restored herdr should still see the pre-restart marker {m1} — the shell survived"
    );

    // The adopted shell is the same live process: a new command runs in it.
    let m2 = "survive_marker_222";
    send_json_request(
        &api_socket,
        "s2",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {m2}\n") }),
    );
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m2, Duration::from_secs(15)),
        "the adopted shell should run a new command and show {m2}"
    );

    drop(herdr_b);
    termhost_send_shutdown(&termhost_socket);
    cleanup_test_base(&base);
}

/// Scenario C: a termhost shell survives a LIVE HANDOFF (in-place binary upgrade).
/// Unlike the restart test (scenario B = SIGKILL + fresh start restoring from the
/// session file), here the running herdr hands its state to a replacement it spawns
/// itself, without killing the persistent daemon. The replacement reconnects to the
/// daemon and ADOPTS the surviving shell — sharing the same reconnect/adopt path as
/// restart, but proving the handoff seam keeps the daemon alive (`handed_off`) rather
/// than tearing it down on the old server's exit.
///
/// This also confirms termhost panes don't break the local-PTY handoff: they own no
/// PTY master fd to pass, so the handoff must skip them while still completing for any
/// fd-backed panes. The replacement's reconnect serial-Attach may briefly block until
/// the old server detaches; `wait_for_api_ready` rides that out.
#[test]
fn termhost_pane_survives_live_handoff() {
    let _lock = test_lock();

    let daemon_bin = match std::env::var("HERDR_TERMHOST_BIN") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP termhost handoff: set HERDR_TERMHOST_BIN to the built Go termhost binary");
            return;
        }
    };
    if !Path::new(&daemon_bin).exists() {
        eprintln!("SKIP termhost handoff: HERDR_TERMHOST_BIN {daemon_bin} not found");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let tmpdir = base.join("tmp");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // --- herdr A: managed daemon + a termhost-backed pane, run a marker ---
    let herdr_a =
        spawn_server_managed(&config_home, &runtime_dir, &api_socket, &daemon_bin, &tmpdir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    let create =
        send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "handoff" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no root_pane.pane_id: {create}"))
        .to_string();

    let managed_socket = termhost_socket_path(&config_home, None);
    assert!(
        wait_until_exists(&managed_socket, Duration::from_secs(10)),
        "herdr should have spawned a persistent daemon at {managed_socket:?}"
    );

    let m1 = "handoff_marker_111";
    send_json_request(
        &api_socket,
        "s1",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {m1}\n") }),
    );
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m1, Duration::from_secs(15)),
        "herdr A's termhost pane should show {m1}"
    );

    // --- live handoff: herdr A spawns its replacement (same binary) and hands over ---
    // The replacement inherits A's environment (XDG_CONFIG_HOME, TMPDIR, HERDR_SOCKET_PATH,
    // HERDR_TERMHOST_BIN), so it rebinds the same API socket and resolves the same daemon.
    let handoff = send_json_request(
        &api_socket,
        "handoff",
        "server.live_handoff",
        json!({}),
    );
    assert!(
        handoff.get("error").is_none(),
        "live handoff should succeed even with a termhost pane present (it owns no PTY \
         master fd, so the handoff must skip it rather than fail): {handoff}"
    );
    drop(herdr_a); // A exits on its own post-handoff; ensure it's reaped if lingering

    // The replacement comes up on the same API socket and reconnects to the daemon.
    assert!(
        wait_for_api_ready(&api_socket, Duration::from_secs(20)),
        "replacement server should rebind the API socket after live handoff"
    );

    // The daemon was kept alive across the handoff (not torn down like a clean quit),
    // so the same live shell is still reachable.
    assert!(
        UnixStream::connect(&managed_socket).is_ok(),
        "the persistent daemon should survive the live handoff and stay reachable at {managed_socket:?}"
    );

    // The pre-handoff output is still in the SAME shell's buffer — the replacement
    // adopted the surviving shell rather than spawning a fresh one.
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m1, Duration::from_secs(20)),
        "the handed-off server should still see the pre-handoff marker {m1} — the shell survived"
    );

    // The adopted shell is the same live process: a new command runs in it.
    let m2 = "handoff_marker_222";
    send_json_request(
        &api_socket,
        "s2",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": format!("echo {m2}\n") }),
    );
    assert!(
        wait_for_pane_text(&api_socket, &pane_id, m2, Duration::from_secs(15)),
        "the adopted shell should run a new command and show {m2}"
    );

    // Clean up: a clean stop of the replacement tears the daemon down (handed_off is
    // false on the new server), so the managed socket should disappear.
    let _ = try_send_json_request(&api_socket, "stop", "server.stop", json!({}));
    termhost_send_shutdown(&managed_socket); // backstop in case the stop raced
    cleanup_test_base(&base);
}

#[test]
fn termhost_pane_reports_agent_identity() {
    let _lock = test_lock();

    let termhost_socket = match std::env::var("HERDR_TERMHOST_SOCKET") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            eprintln!("SKIP termhost_e2e: HERDR_TERMHOST_SOCKET unset");
            return;
        }
    };
    if UnixStream::connect(&termhost_socket).is_err() {
        eprintln!("SKIP termhost_e2e: no daemon reachable at {termhost_socket}");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &termhost_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    let create = send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "ag" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no root_pane.pane_id: {create}"))
        .to_string();

    // Replace the shell with a process advertising argv[0]="claude" (a real binary
    // under a fake name). Go's procscan inspects the foreground process group and
    // reports the agent over the seam; Rust maps it onto detected agent state.
    let resp = send_json_request(
        &api_socket,
        "agent",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": "exec -a claude sleep 30\n" }),
    );
    assert!(resp.get("error").is_none(), "pane.send_text failed: {resp}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_agent = String::new();
    while Instant::now() < deadline {
        let info = send_json_request(&api_socket, "get", "pane.get", json!({ "pane_id": pane_id }));
        if let Some(agent) = info["result"]["pane"]["agent"].as_str() {
            got_agent = agent.to_string();
            if agent == "claude" {
                break;
            }
        }
        thread::sleep(Duration::from_millis(150));
    }
    assert_eq!(
        got_agent, "claude",
        "Go-side detection should report agent identity to the termhost pane (pane.get agent), got {got_agent:?}"
    );

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn termhost_pane_reports_agent_working_state() {
    let _lock = test_lock();

    let termhost_socket = match std::env::var("HERDR_TERMHOST_SOCKET") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            eprintln!("SKIP termhost_e2e: HERDR_TERMHOST_SOCKET unset");
            return;
        }
    };
    if UnixStream::connect(&termhost_socket).is_err() {
        eprintln!("SKIP termhost_e2e: no daemon reachable at {termhost_socket}");
        return;
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &termhost_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    let create = send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "wk" }));
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no root_pane.pane_id: {create}"))
        .to_string();

    // Become a process named "pi" (agent) that continuously prints the pi
    // manifest's working marker, so Go classifies state=working via the manifest.
    let resp = send_json_request(
        &api_socket,
        "work",
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": "exec -a pi sh -c 'while :; do printf \"Working...\"; sleep 1; done'\n" }),
    );
    assert!(resp.get("error").is_none(), "pane.send_text failed: {resp}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = json!(null);
    while Instant::now() < deadline {
        let info = send_json_request(&api_socket, "get", "pane.get", json!({ "pane_id": pane_id }));
        let agent = info["result"]["pane"]["agent"].as_str().unwrap_or("");
        let status = info["result"]["pane"]["agent_status"].as_str().unwrap_or("");
        last = info["result"]["pane"].clone();
        if agent == "pi" && status == "working" {
            drop(spawned);
            cleanup_test_base(&base);
            return;
        }
        thread::sleep(Duration::from_millis(150));
    }
    drop(spawned);
    cleanup_test_base(&base);
    panic!("expected agent=pi status=working from manifest detection; last pane = {last}");
}
