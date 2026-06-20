//! Rust client for the Phase B Go↔Rust orchestration seam.
//!
//! This mirrors the Go `internal/orchestration` protocol: length-prefixed JSON
//! frames (`[u32-LE len][payload]`) carrying commands (Rust→Go) and events
//! (Go→Rust). Pane IDs are `u32`; frames match herdr `wire::FrameData`/`CellData`
//! (packed `0x02_RR_GG_BB` colors, ratatui modifier bits, `skip` diffing).
//!
//! See ai_docs/phase-b-orchestration-seam.md in the herdr-web repo for the design.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to the message shapes. Must match the Go side.
pub const PROTOCOL_VERSION: i32 = 1;

/// Caps a single length-prefixed frame (matches the Go side).
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

/// Commands sent Rust → Go. Serialized with an internal `"type"` tag matching
/// the Go message types (`hello`, `create_pane`, `input`, `resize`, `close_pane`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Hello {
        protocol_version: i32,
    },
    CreatePane {
        pane_id: u32,
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        cwd: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
    },
    Input {
        pane_id: u32,
        /// Raw bytes for the pane's PTY. Encoded as base64 to match Go's
        /// `json:"data"` on a `[]byte` field.
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Resize {
        pane_id: u32,
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
    },
    ClosePane {
        pane_id: u32,
    },
}

impl Command {
    pub fn hello() -> Self {
        Command::Hello { protocol_version: PROTOCOL_VERSION }
    }
}

/// Events received Go → Rust.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Welcome {
        protocol_version: i32,
        #[serde(default)]
        error: String,
    },
    PaneFrame {
        pane_id: u32,
        frame: Frame,
    },
    PaneExited {
        pane_id: u32,
        exit_code: i32,
    },
    Error {
        #[serde(default)]
        pane_id: u32,
        message: String,
    },
}

/// One pane's grid, full or diffed. Mirrors herdr `wire::FrameData`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub cursor: Option<Cursor>,
    pub cells: Vec<Cell>,
}

/// Mirrors herdr `wire::CellData`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    pub symbol: String,
    pub fg: u32,
    pub bg: u32,
    pub modifier: u16,
    pub skip: bool,
    pub hyperlink: Option<u32>,
}

/// Mirrors herdr `wire::CursorState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub shape: u8,
}

/// A connection to the Go termhost over any read/write stream.
pub struct Conn<S> {
    stream: S,
}

impl<S: Read + Write> Conn<S> {
    pub fn new(stream: S) -> Self {
        Conn { stream }
    }

    /// Sends one command as a length-prefixed JSON frame.
    pub fn send(&mut self, cmd: &Command) -> io::Result<()> {
        let payload = serde_json::to_vec(cmd).map_err(io_err)?;
        if payload.len() > MAX_FRAME_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        self.stream.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.stream.write_all(&payload)?;
        self.stream.flush()
    }

    /// Reads one event frame.
    pub fn recv(&mut self) -> io::Result<Event> {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr)?;
        let n = u32::from_le_bytes(hdr) as usize;
        if n > MAX_FRAME_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf)?;
        serde_json::from_slice(&buf).map_err(io_err)
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Decodes a packed color (`0x02_RR_GG_BB`) into `Some((r,g,b))`, or `None` for
/// the non-RGB encodings (named / palette).
pub fn unpack_rgb(v: u32) -> Option<(u8, u8, u8)> {
    if (v >> 24) == 0x02 {
        Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
    } else {
        None
    }
}

/// base64 (de)serialization for `[]byte`/`Vec<u8>` fields, matching Go's
/// `encoding/json` standard-base64 handling.
mod b64 {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_data_is_base64() {
        // Go encodes []byte as standard base64; "hi" -> "aGk=".
        let cmd = Command::Input { pane_id: 1, data: b"hi".to_vec() };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""data":"aGk=""#), "got {json}");
    }

    #[test]
    fn command_type_tags_match_go() {
        let j = serde_json::to_string(&Command::hello()).unwrap();
        assert!(j.contains(r#""type":"hello""#), "{j}");
        let j = serde_json::to_string(&Command::CreatePane {
            pane_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            cwd: String::new(),
            command: String::new(),
            args: vec![],
            env: Default::default(),
        })
        .unwrap();
        assert!(j.contains(r#""type":"create_pane""#), "{j}");
        // optional empty fields are omitted
        assert!(!j.contains("cwd"), "{j}");
    }

    #[test]
    fn decode_pane_frame_event() {
        let raw = r#"{"type":"pane_frame","pane_id":3,"frame":{"cols":1,"rows":1,"full":true,"cursor":{"x":0,"y":0,"visible":true,"shape":2},"cells":[{"symbol":"h","fg":33685555,"bg":33554432,"modifier":1,"skip":false,"hyperlink":null}]}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        match ev {
            Event::PaneFrame { pane_id, frame } => {
                assert_eq!(pane_id, 3);
                assert_eq!(frame.cells.len(), 1);
                assert_eq!(frame.cells[0].symbol, "h");
                assert_eq!(frame.cells[0].modifier, 1);
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn unpack_rgb_works() {
        assert_eq!(unpack_rgb(0x02_12_34_56), Some((0x12, 0x34, 0x56)));
        assert_eq!(unpack_rgb(0x00_00_00_01), None); // named color
    }
}
