//! Wire contract for the Phase B Go↔Rust orchestration seam, from the Rust
//! (orchestrator) side. Length-prefixed JSON frames (`[u32-LE len][payload]`)
//! carrying commands (Rust→Go) and events (Go→Rust).
//!
//! Frames deserialize their `cells`/`cursor` straight into herdr's own
//! [`wire::CellData`]/[`wire::CursorState`] (the field names already match), so a
//! termhost frame converts to a [`wire::FrameData`] with no per-cell copying.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::protocol as wire;

/// Bumped on any breaking change to the message shapes. Must match the Go side.
pub const PROTOCOL_VERSION: i32 = 1;

/// Caps a single length-prefixed frame (matches the Go side).
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

/// Commands sent Rust → Go.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Hello {
        protocol_version: i32,
    },
    CreatePane {
        pane_id: u32,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        #[serde(skip_serializing_if = "String::is_empty")]
        cwd: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        command: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Input {
        pane_id: u32,
        /// Raw PTY bytes, base64-encoded to match Go's `json:"data"` on `[]byte`.
        #[serde(serialize_with = "b64_serialize")]
        data: Vec<u8>,
    },
    Resize {
        pane_id: u32,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    },
    ClosePane {
        pane_id: u32,
    },
}

/// Events received Go → Rust.
#[derive(Debug, Clone, Deserialize)]
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
    PaneCwd {
        pane_id: u32,
        cwd: String,
    },
    PaneAgent {
        pane_id: u32,
        #[serde(default)]
        agent: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        visible_blocker: bool,
        #[serde(default)]
        visible_working: bool,
    },
    PaneClipboard {
        pane_id: u32,
        /// Decoded clipboard bytes (base64 on the wire, matching Go's `[]byte`).
        /// Empty is a clipboard-clear.
        #[serde(deserialize_with = "b64_deserialize")]
        data: Vec<u8>,
    },
    PaneTitle {
        pane_id: u32,
        /// OSC 0/2 window title; empty is a title-clear.
        #[serde(default)]
        title: String,
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

/// One pane's grid, full or diffed. `cells`/`cursor` reuse herdr's own types.
#[derive(Debug, Clone, Deserialize)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub cursor: Option<wire::CursorState>,
    pub cells: Vec<wire::CellData>,
}

impl Frame {
    /// Converts into herdr's render frame. Hyperlinks/graphics are not carried by
    /// the seam yet (reserved), so they are empty.
    pub fn into_frame_data(self) -> wire::FrameData {
        wire::FrameData {
            cells: self.cells,
            width: self.cols,
            height: self.rows,
            cursor: self.cursor,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }
}

fn b64_serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(bytes))
}

fn b64_deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    STANDARD.decode(s.as_bytes()).map_err(serde::de::Error::custom)
}

fn invalid<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Writes one command as a length-prefixed JSON frame.
pub fn write_command<W: Write>(w: &mut W, cmd: &Command) -> io::Result<()> {
    let payload = serde_json::to_vec(cmd).map_err(invalid)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(invalid("command frame too large"));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Reads one event frame.
pub fn read_event<R: Read>(r: &mut R) -> io::Result<Event> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n > MAX_FRAME_SIZE {
        return Err(invalid("event frame too large"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_data_is_base64() {
        let cmd = Command::Input { pane_id: 1, data: b"hi".to_vec() };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""data":"aGk=""#), "{json}");
        assert!(json.contains(r#""type":"input""#), "{json}");
    }

    #[test]
    fn create_pane_omits_empty_optionals() {
        let cmd = Command::CreatePane {
            pane_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            cwd: String::new(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""type":"create_pane""#), "{json}");
        assert!(!json.contains("cwd") && !json.contains("command"), "{json}");
    }

    #[test]
    fn frame_event_decodes_into_herdr_types() {
        let raw = r#"{"type":"pane_frame","pane_id":3,"frame":{"cols":2,"rows":1,"full":true,"cursor":{"x":1,"y":0,"visible":true,"shape":6},"cells":[{"symbol":"h","fg":33685555,"bg":33554432,"modifier":1,"skip":false,"hyperlink":null},{"symbol":" ","fg":33554431,"bg":33554432,"modifier":0,"skip":true,"hyperlink":null}]}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        let frame = match ev {
            Event::PaneFrame { pane_id, frame } => {
                assert_eq!(pane_id, 3);
                frame
            }
            other => panic!("wrong event: {other:?}"),
        };
        let fd = frame.into_frame_data();
        assert_eq!(fd.width, 2);
        assert_eq!(fd.height, 1);
        assert_eq!(fd.cells.len(), 2);
        assert_eq!(fd.cells[0].symbol, "h");
        assert_eq!(fd.cells[0].modifier, 1);
        assert!(fd.cells[1].skip);
        let cur = fd.cursor.expect("cursor");
        assert_eq!((cur.x, cur.y, cur.visible, cur.shape), (1, 0, true, 6));
        assert!(fd.hyperlinks.is_empty());
    }

    #[test]
    fn pane_cwd_decodes() {
        let ev: Event =
            serde_json::from_str(r#"{"type":"pane_cwd","pane_id":5,"cwd":"/tmp/work"}"#).unwrap();
        match ev {
            Event::PaneCwd { pane_id, cwd } => {
                assert_eq!(pane_id, 5);
                assert_eq!(cwd, "/tmp/work");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn pane_clipboard_decodes_base64() {
        let ev: Event =
            serde_json::from_str(r#"{"type":"pane_clipboard","pane_id":6,"data":"aGVsbG8="}"#)
                .unwrap();
        match ev {
            Event::PaneClipboard { pane_id, data } => {
                assert_eq!(pane_id, 6);
                assert_eq!(data, b"hello");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn pane_clipboard_empty_is_clear() {
        let ev: Event =
            serde_json::from_str(r#"{"type":"pane_clipboard","pane_id":6,"data":""}"#).unwrap();
        match ev {
            Event::PaneClipboard { pane_id, data } => {
                assert_eq!(pane_id, 6);
                assert!(data.is_empty());
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn pane_title_decodes() {
        let ev: Event =
            serde_json::from_str(r#"{"type":"pane_title","pane_id":7,"title":"vim - main.go"}"#)
                .unwrap();
        match ev {
            Event::PaneTitle { pane_id, title } => {
                assert_eq!(pane_id, 7);
                assert_eq!(title, "vim - main.go");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn welcome_decodes_with_default_error() {
        let ev: Event = serde_json::from_str(r#"{"type":"welcome","protocol_version":1}"#).unwrap();
        match ev {
            Event::Welcome { protocol_version, error } => {
                assert_eq!(protocol_version, 1);
                assert!(error.is_empty());
            }
            other => panic!("wrong event: {other:?}"),
        }
    }
}
