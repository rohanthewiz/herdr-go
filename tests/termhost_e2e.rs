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
//! the step-3 render path. (Note: `pane.read` reads the local, unfed emulator for
//! termhost panes, so the assertion goes through the rendered client frame.)
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

fn send_json_request(socket_path: &Path, id: &str, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    let request = json!({ "id": id, "method": method, "params": params });
    writeln!(stream, "{request}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("response should be valid JSON")
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
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create should return root_pane.pane_id: {create}"))
        .to_string();

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

    // Definitive proof this went through the Go backend (not an in-process
    // fallback): for a termhost pane the local Rust emulator is unfed, so
    // `pane.read` (which reads that local emulator) must NOT contain the marker
    // even though the rendered frame does. An in-process pane would show it here.
    let local_read = send_json_request(
        &api_socket,
        "read",
        "pane.read",
        json!({ "pane_id": pane_id, "source": "recent", "lines": 200 }),
    );
    let local_text = local_read["result"]["read"]["text"].as_str().unwrap_or_default();
    eprintln!("termhost_e2e: pane.read (local emulator) len={}", local_text.len());
    assert!(
        !local_text.contains(marker),
        "termhost pane's local emulator should be unfed (degraded), but pane.read contained '{marker}' \
         — the pane is NOT termhost-backed (in-process fallback?)"
    );

    drop(spawned);
    cleanup_test_base(&base);
}
