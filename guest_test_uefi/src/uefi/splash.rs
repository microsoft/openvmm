// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// NOTE: adapted from uefi-rs sierpinski example code:
// https://github.com/rust-osdev/uefi-rs/blob/main/uefi-test-runner/examples/sierpinski.rs

use alloc::vec::Vec;
use core::num::NonZeroU8;
use uefi::Result;
use uefi::boot;
use uefi::proto::console::gop::BltOp;
use uefi::proto::console::gop::BltPixel;
use uefi::proto::console::gop::BltRegion;
use uefi::proto::console::gop::GraphicsOutput;

struct Buffer {
    width: usize,
    height: usize,
    pixels: Vec<BltPixel>,
}

impl Buffer {
    /// Create a new `Buffer`.
    fn new(width: usize, height: usize) -> Self {
        Buffer {
            width,
            height,
            pixels: vec![BltPixel::new(0, 0, 0); width * height],
        }
    }

    /// Get a single pixel.
    fn pixel(&mut self, x: usize, y: usize) -> Option<&mut BltPixel> {
        self.pixels.get_mut(y * self.width + x)
    }

    /// Blit the buffer to the framebuffer.
    fn blit(&self, gop: &mut GraphicsOutput) -> Result {
        gop.blt(BltOp::BufferToVideo {
            buffer: &self.pixels,
            src: BltRegion::Full,
            dest: (0, 0),
            dims: (self.width, self.height),
        })
    }
}

pub struct Splashes(pub NonZeroU8);

pub fn draw_splash(splashes: Splashes) {
    // The graphic output is not always available.
    let gop_handle = if let Ok(handle) = boot::get_handle_for_protocol::<GraphicsOutput>() {
        handle
    } else {
        return;
    };
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle).expect("can get GOP");

    // Create a buffer to draw into.
    let (resolution_width, resolution_height) = gop.current_mode_info().resolution();
    let mut buffer = Buffer::new(resolution_width, resolution_height);

    let splashes = splashes.0.get() as usize;
    let height = resolution_height / splashes;
    let width = resolution_width / splashes;
    for s in 0..splashes {
        // Initialize the buffer with a simple gradient background.
        for y in s * height..(s + 1) * height {
            let r = ((y as f32) / ((resolution_height - 1) as f32)) * 255.0;
            for x in s * width..(s + 1) * width {
                let g = ((x as f32) / ((resolution_width - 1) as f32)) * 255.0;
                let pixel = buffer.pixel(x, y).expect("Can draw a pixel");
                pixel.red = r as u8;
                pixel.green = g as u8;
                pixel.blue = 255;
            }
        }
    }

    // Draw the buffer to the screen.
    buffer.blit(&mut gop).expect("can draw the image");
}
