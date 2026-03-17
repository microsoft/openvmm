// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio console spec constants and configuration types.

/// Virtio console device ID.
pub const VIRTIO_DEVICE_ID_CONSOLE: u16 = 3;

/// Feature bit: console size (cols, rows) is available in config space.
pub const VIRTIO_CONSOLE_F_SIZE: u64 = 0;

/// Virtio console configuration space layout.
///
/// From the virtio spec §5.3.4:
/// - cols: u16 at offset 0
/// - rows: u16 at offset 2
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, inspect::Inspect)]
pub struct VirtioConsoleConfig {
    pub cols: u16,
    pub rows: u16,
}

impl VirtioConsoleConfig {
    pub fn read_u32(&self, offset: u16) -> u32 {
        match offset {
            // cols at bytes 0..2, rows at bytes 2..4 — both fit in one u32 read
            0 => (self.cols as u32) | ((self.rows as u32) << 16),
            _ => {
                tracelimit::warn_ratelimited!(offset, "invalid config read offset");
                0
            }
        }
    }
}
