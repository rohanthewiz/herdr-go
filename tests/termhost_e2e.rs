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
fn termhost_managed_daemon_spawns_and_is_supervised() {
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
    let herdr_pid = spawned.child.process_id();
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Create a workspace → herdr lazily spawns the managed daemon for the root pane.
    let create =
        send_json_request(&api_socket, "ws", "workspace.create", json!({ "label": "managed" }));
    assert!(create.get("error").is_none(), "workspace.create failed: {create}");
    let pane_id = create["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no root_pane.pane_id: {create}"))
        .to_string();

    let _ = pane_id;

    // The managed socket (named by herdr's pid in TMPDIR) appears once herdr spawns
    // the daemon for the pane — proof the orchestrator launched and connected to a
    // daemon it manages (the socket path is one only herdr knows, from the binary +
    // pid). This is the lifecycle contract this test owns; the per-pane render path
    // is covered by the other e2e tests.
    let managed_socket = herdr_pid
        .map(|pid| tmpdir.join(format!("herdr-termhost-{pid}.sock")))
        .expect("herdr child should report a pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !managed_socket.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        managed_socket.exists(),
        "herdr should have spawned a managed daemon at {managed_socket:?} after pane creation"
    );

    // Supervision backstop: kill herdr; the daemon's --exit-on-disconnect must make
    // it exit and remove its socket, so no orphaned daemon lingers.
    drop(spawned); // SIGKILLs herdr — no graceful shutdown() runs, exercising the backstop
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && managed_socket.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !managed_socket.exists(),
        "daemon should exit and remove its socket after the orchestrator dies, but {managed_socket:?} remains"
    );

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
