// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits implemented by the embedder to plug into the VNC server.

/// A trait used to retrieve data from a framebuffer.
pub trait Framebuffer: Send + Sync {
    fn resolution(&mut self) -> (u16, u16);
    fn read_line(&mut self, line: u16, data: &mut [u8]);
}

pub(crate) const HID_MOUSE_MAX_ABS_VALUE: u32 = 0x7FFF;

/// A trait used to handle VNC client input.
pub trait Input {
    fn key(&mut self, scancode: u16, is_down: bool);
    fn mouse(&mut self, button_mask: u8, x: u16, y: u16);
}
