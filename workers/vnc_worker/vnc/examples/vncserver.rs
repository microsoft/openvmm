// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is a small test program of the VNC server functionality.

use pal_async::local::block_with_io;
use pal_async::socket::PolledSocket;
use std::net::TcpListener;
use vnc::Error;
use zerocopy::IntoBytes;

fn pixel(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

const WIDTH: usize = 800;
const HEIGHT: usize = 600;

struct Framebuffer(Vec<u32>);

impl vnc::Framebuffer for Framebuffer {
    fn resolution(&mut self) -> (u16, u16) {
        (WIDTH as u16, HEIGHT as u16)
    }

    fn read_line(&mut self, line: u16, data: &mut [u8]) {
        let start = (line as usize) * WIDTH;
        data.copy_from_slice(&self.0.as_bytes()[start..start + WIDTH * 4]);
    }
}

struct IgnoreInput;

impl vnc::Input for IgnoreInput {
    fn key(&mut self, _scancode: u16, _is_down: bool) {}
    fn mouse(&mut self, _button_mask: u8, _x: u16, _y: u16) {}
}

fn main() -> Result<(), Error> {
    block_with_io(|driver| async move {
        let light_grey = pixel(127, 127, 127);
        let dark_grey = pixel(63, 63, 63);
        // Checkerboard pattern.
        let mut fb = vec![light_grey; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if (y / 32) % 2 == (x / 32) % 2 {
                    fb[y * WIDTH + x] = dark_grey;
                }
            }
        }
        let fb = Framebuffer(fb);

        let mut listener = PolledSocket::new(&driver, TcpListener::bind("127.0.0.1:5900")?)?;
        let (socket, _addr) = listener.accept().await?;
        let socket = PolledSocket::new(&driver, socket.into())?;
        let mut server = vnc::Server::new("test framebuffer".into(), socket, fb, IgnoreInput);
        server.run().await
    })
}
