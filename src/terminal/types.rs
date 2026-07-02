//! Ghostty-free plain-data terminal types shared across the app, termhost,
//! and (until WS0 stage D) the in-process emulator.
//!
//! These used to live in `src/ghostty/mod.rs`, which leaked the ghostty module
//! onto the app/termhost surface. They carry no FFI: focus reporting is a
//! fixed CSI pair, and the Kitty image structs are pure descriptions of
//! decoded placements.

/// Terminal focus change, reported to the child application when focus
/// reporting (DEC mode 1004) is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    Gained,
    Lost,
}

/// Encodes a focus event as the escape sequence sent to the child: CSI I on
/// gain, CSI O on loss (matches libghostty-vt's `focus_encode`).
pub fn encode_focus(event: FocusEvent) -> Vec<u8> {
    match event {
        FocusEvent::Gained => b"\x1b[I".to_vec(),
        FocusEvent::Lost => b"\x1b[O".to_vec(),
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum KittyImageFormat {
    Rgb,
    Rgba,
    Png,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub z: i32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub format: KittyImageFormat,
    pub data_len: usize,
    pub data_fingerprint: u64,
    pub data: Vec<u8>,
    pub render: KittyPlacementRenderInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageDescriptor {
    pub image_id: u32,
    pub placement_id: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub format: KittyImageFormat,
    pub data_len: usize,
    pub data_fingerprint: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacementRenderInfo {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_focus_matches_libghostty_vt_sequences() {
        // Pinned by vendor/libghostty-vt/src/terminal/c/focus.zig tests.
        assert_eq!(encode_focus(FocusEvent::Gained), b"\x1b[I");
        assert_eq!(encode_focus(FocusEvent::Lost), b"\x1b[O");
    }
}
