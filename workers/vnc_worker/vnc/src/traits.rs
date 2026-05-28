// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits implemented by the embedder to plug into the VNC server.

/// A trait used to retrieve data from a framebuffer.
pub trait Framebuffer: Send + Sync {
    /// Returns the current framebuffer resolution as `(width, height)` in
    /// pixels. Called once per update cycle so the server can react to
    /// guest-driven resolution changes.
    fn resolution(&mut self) -> (u16, u16);
    /// Reads the pixel data for one scanline (`line`, 0-based) into `data`.
    /// `data` is sized by the caller for one full scanline at the current
    /// resolution in the internal `0x00RRGGBB` format.
    fn read_line(&mut self, line: u16, data: &mut [u8]);
}

pub(crate) const HID_MOUSE_MAX_ABS_VALUE: u32 = 0x7FFF;

/// A trait used to handle VNC client input.
pub trait Input {
    /// Reports a keyboard event. `scancode` is the AT scancode (set 1);
    /// `is_down` is true for press, false for release.
    fn key(&mut self, scancode: u16, is_down: bool);
    /// Reports a mouse event. `button_mask` is the RFB button bitmap; `x`
    /// and `y` are pixel coordinates within the framebuffer.
    fn mouse(&mut self, button_mask: u8, x: u16, y: u16);
}
