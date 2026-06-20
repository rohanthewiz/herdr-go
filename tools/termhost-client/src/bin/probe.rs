//! Cross-language probe: drives the Go `termhost` daemon over the orchestration
//! seam from Rust and prints what comes back. Proves the protocol round-trips
//! between the Rust orchestrator side and the Go terminal backend.
//!
//! Usage:
//!   termhost-probe [socket-path]   (default /tmp/herdr-termhost.sock)

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use termhost_client::{unpack_rgb, Command, Conn, Event, Frame};

fn main() {
    let socket = std::env::args().nth(1).unwrap_or_else(|| "/tmp/herdr-termhost.sock".into());
    if let Err(e) = run(&socket) {
        eprintln!("termhost-probe: {e}");
        std::process::exit(1);
    }
}

fn run(socket: &str) -> std::io::Result<()> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| std::io::Error::new(e.kind(), format!("connect {socket}: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut conn = Conn::new(stream);

    // Handshake.
    conn.send(&Command::hello())?;
    match conn.recv()? {
        Event::Welcome { protocol_version, error } => {
            if !error.is_empty() {
                return Err(std::io::Error::other(format!("welcome error: {error}")));
            }
            println!("connected: termhost protocol v{protocol_version}");
        }
        other => return Err(std::io::Error::other(format!("expected welcome, got {other:?}"))),
    }

    // --- Test 1: run a command, collect frames until it exits. ---
    println!("\n=== test 1: create_pane running a shell command ===");
    conn.send(&Command::CreatePane {
        pane_id: 1,
        cols: 40,
        rows: 6,
        cell_width_px: 0,
        cell_height_px: 0,
        cwd: String::new(),
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf 'RUST<->GO\\n'; printf '\\033[31mRED\\033[0m and \\033[1mBOLD\\033[0m\\n'".into(),
        ],
        env: BTreeMap::new(),
    })?;

    let mut last_frame: Option<Frame> = None;
    loop {
        match conn.recv()? {
            Event::PaneFrame { pane_id, frame } => {
                println!("  pane {pane_id}: frame {}x{} full={} cells={}", frame.cols, frame.rows, frame.full, frame.cells.len());
                last_frame = Some(frame);
            }
            Event::PaneExited { pane_id, exit_code } => {
                println!("  pane {pane_id}: exited code={exit_code}");
                break;
            }
            Event::Error { pane_id, message } => {
                return Err(std::io::Error::other(format!("pane {pane_id} error: {message}")));
            }
            other => println!("  (ignoring {other:?})"),
        }
    }
    if let Some(f) = &last_frame {
        print_frame(f);
    } else {
        return Err(std::io::Error::other("never received a frame for pane 1"));
    }

    // --- Test 2: input echo + close. ---
    println!("\n=== test 2: input echo through a cat pane, then close ===");
    conn.send(&Command::CreatePane {
        pane_id: 2,
        cols: 40,
        rows: 4,
        cell_width_px: 0,
        cell_height_px: 0,
        cwd: String::new(),
        command: "/bin/cat".into(),
        args: vec![],
        env: BTreeMap::new(),
    })?;
    conn.send(&Command::Input { pane_id: 2, data: b"ping from rust\r".to_vec() })?;

    let mut saw_echo = false;
    for _ in 0..200 {
        match conn.recv()? {
            Event::PaneFrame { pane_id: 2, frame } => {
                if frame_text(&frame).contains("ping from rust") {
                    println!("  saw echoed input in pane 2 frame");
                    saw_echo = true;
                    break;
                }
            }
            Event::Error { pane_id, message } => {
                return Err(std::io::Error::other(format!("pane {pane_id} error: {message}")));
            }
            _ => {}
        }
    }
    if !saw_echo {
        return Err(std::io::Error::other("never saw echoed input in pane 2"));
    }

    conn.send(&Command::ClosePane { pane_id: 2 })?;
    loop {
        match conn.recv()? {
            Event::PaneExited { pane_id: 2, exit_code } => {
                println!("  pane 2 closed, exit code={exit_code}");
                break;
            }
            Event::Error { pane_id, message } => {
                return Err(std::io::Error::other(format!("pane {pane_id} error: {message}")));
            }
            _ => {}
        }
    }

    println!("\nOK: Rust client ↔ Go termhost round-trip verified.");
    Ok(())
}

fn frame_text(f: &Frame) -> String {
    f.cells.iter().map(|c| c.symbol.as_str()).collect()
}

fn print_frame(f: &Frame) {
    println!("  --- rendered grid (trailing blanks trimmed) ---");
    let cols = f.cols as usize;
    for (r, row) in f.cells.chunks(cols).enumerate() {
        let mut line: String = row.iter().map(|c| c.symbol.as_str()).collect();
        while line.ends_with(' ') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        // Annotate any explicitly-colored / styled cells in this row.
        let mut notes = Vec::new();
        for (x, c) in row.iter().enumerate() {
            if c.symbol != " " && !c.symbol.is_empty() {
                if let Some((cr, cg, cb)) = unpack_rgb(c.fg) {
                    if c.modifier != 0 || (cr, cg, cb) != default_fg(f) {
                        notes.push(format!("[{x}]{:?} #{cr:02x}{cg:02x}{cb:02x} mod={}", c.symbol, c.modifier));
                    }
                }
            }
        }
        println!("  row {r}: {line:?}");
        for n in notes {
            println!("         {n}");
        }
    }
    if let Some(cur) = &f.cursor {
        println!("  cursor: ({},{}) visible={} shape={}", cur.x, cur.y, cur.visible, cur.shape);
    }
}

// The first cell's resolved fg is the pane default; used only to decide which
// cells are "interestingly" colored for the annotation above.
fn default_fg(f: &Frame) -> (u8, u8, u8) {
    f.cells
        .first()
        .and_then(|c| unpack_rgb(c.fg))
        .unwrap_or((0, 0, 0))
}
