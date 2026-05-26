// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VNC server implementation.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod rfb;
mod scancode;
use flate2::Compression;
use flate2::FlushCompress;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::OptionFuture;
use pal_async::socket::PolledSocket;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const TILE_SIZE: u16 = 64;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported protocol version")]
    UnsupportedVersion(rfb::ProtocolVersion),
    #[error("unsupported message type: {0:#x}")]
    UnknownMessage(u8),
    #[error("unsupported qemu message type: {0:#x}")]
    UnknownQemuMessage(u8),
    #[error("unsupported pixel format: {0} bits per pixel")]
    UnsupportedPixelFormat(u8),
    #[error("unsupported security type: {0}")]
    UnsupportedSecurityType(u8),
    #[error("resolution changed but client does not support DesktopSize")]
    ResizeUnsupported,
    #[error("zlib compression failed")]
    ZlibCompression,
    #[error("socket error")]
    Io(#[from] std::io::Error),
}

/// A trait used to retrieve data from a framebuffer.
pub trait Framebuffer: Send + Sync {
    fn resolution(&mut self) -> (u16, u16);
    fn read_line(&mut self, line: u16, data: &mut [u8]);
}

pub const HID_MOUSE_MAX_ABS_VALUE: u32 = 0x7FFFu32;

/// A VNC server handling a single connection.
pub struct Server<F, I> {
    socket: PolledSocket<socket2::Socket>,
    fb: F,
    input: I,
    update_recv: mpsc::Receiver<()>,
    update_send: mpsc::Sender<()>,
    name: String,

    // ctrl-alt-p paste intercept
    ctrl_left_pressed: bool,
    alt_left_pressed: bool,
    clipboard: String,

    supports_desktop_resize: bool,
    supports_zlib: bool,
    supports_cursor: bool,
    prev_fb: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Updater(mpsc::Sender<()>);

impl Updater {
    pub fn update(&self) {
        let _ = self.0.clone().try_send(());
    }
}

/// A trait used to handle VNC client input.
pub trait Input {
    fn key(&mut self, scancode: u16, is_down: bool);
    fn mouse(&mut self, button_mask: u8, x: u16, y: u16);
}

/// Convert source pixels to the client's pixel format and append to `out`.
fn convert_pixels(src: &[u32], fmt: &rfb::PixelFormat, out: &mut Vec<u8>) {
    let dest_depth = fmt.bits_per_pixel as usize / 8;
    let shift_r = 24 - fmt.red_max.get().count_ones();
    let shift_g = 16 - fmt.green_max.get().count_ones();
    let shift_b = 8 - fmt.red_max.get().count_ones();
    let big_endian = fmt.big_endian_flag != 0;
    let no_convert = dest_depth == 4
        && !big_endian
        && shift_r == fmt.red_shift as u32
        && shift_g == fmt.green_shift as u32
        && shift_b == fmt.blue_shift as u32;

    if no_convert {
        out.extend_from_slice(src.as_bytes());
        return;
    }

    for &p in src {
        let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
        let p2 = r >> shift_r << fmt.red_shift
            | g >> shift_g << fmt.green_shift
            | b >> shift_b << fmt.blue_shift;
        match (dest_depth, big_endian) {
            (1, _) => out.push(p2 as u8),
            (2, false) => out.extend_from_slice(&(p2 as u16).to_le_bytes()),
            (2, true) => out.extend_from_slice(&(p2 as u16).to_be_bytes()),
            (4, false) => out.extend_from_slice(&p2.to_le_bytes()),
            (4, true) => out.extend_from_slice(&p2.to_be_bytes()),
            _ => unreachable!(),
        }
    }
}

impl<F: Framebuffer, I: Input> Server<F, I> {
    pub fn new(
        name: String,
        socket: PolledSocket<socket2::Socket>,
        fb: F,
        input: I,
    ) -> Server<F, I> {
        #[expect(clippy::disallowed_methods)] // TODO
        let (update_send, update_recv) = mpsc::channel(1);
        Self {
            socket,
            fb,
            input,
            update_recv,
            update_send,
            name,

            ctrl_left_pressed: false,
            alt_left_pressed: false,
            clipboard: String::new(),
            supports_desktop_resize: false,
            supports_zlib: false,
            supports_cursor: false,
            prev_fb: Vec::new(),
        }
    }

    pub fn updater(&mut self) -> Updater {
        Updater(self.update_send.clone())
    }

    pub fn done(self) -> (F, I) {
        (self.fb, self.input)
    }

    /// Runs the VNC server.
    pub async fn run(&mut self) -> Result<(), Error> {
        match self.run_internal().await {
            Ok(()) => Ok(()),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::ConnectionReset => Ok(()),
            err => err,
        }
    }

    async fn run_internal(&mut self) -> Result<(), Error> {
        let socket = &mut self.socket;
        socket
            .write_all(rfb::ProtocolVersion(rfb::PROTOCOL_VERSION_38).as_bytes())
            .await?;

        let mut version = rfb::ProtocolVersion::new_zeroed();
        socket.read_exact(version.as_mut_bytes()).await?;

        match version.0 {
            rfb::PROTOCOL_VERSION_33 => {
                // RFB 3.3: server dictates security type as a u32.
                socket
                    .write_all(
                        rfb::Security33 {
                            padding: [0; 3],
                            security_type: rfb::SECURITY_TYPE_NONE,
                        }
                        .as_bytes(),
                    )
                    .await?;
            }
            rfb::PROTOCOL_VERSION_37 | rfb::PROTOCOL_VERSION_38 => {
                // RFB 3.7/3.8: server sends a list of supported security types.
                socket
                    .write_all(rfb::Security37 { type_count: 1 }.as_bytes())
                    .await?;
                socket.write_all(&[rfb::SECURITY_TYPE_NONE]).await?;

                // Client responds with chosen security type.
                let mut chosen_type = 0u8;
                socket.read_exact(chosen_type.as_mut_bytes()).await?;

                if chosen_type != rfb::SECURITY_TYPE_NONE {
                    if version.0 == rfb::PROTOCOL_VERSION_38 {
                        socket
                            .write_all(
                                rfb::SecurityResult {
                                    status: rfb::SECURITY_RESULT_STATUS_FAILED.into(),
                                }
                                .as_bytes(),
                            )
                            .await?;
                    }
                    return Err(Error::UnsupportedSecurityType(chosen_type));
                }

                if version.0 == rfb::PROTOCOL_VERSION_38 {
                    // RFB 3.8: server sends SecurityResult after negotiation.
                    socket
                        .write_all(
                            rfb::SecurityResult {
                                status: rfb::SECURITY_RESULT_STATUS_OK.into(),
                            }
                            .as_bytes(),
                        )
                        .await?;
                }
            }
            _ => return Err(Error::UnsupportedVersion(version)),
        }

        let mut init = rfb::ClientInit::new_zeroed();
        socket.read_exact(init.as_mut_bytes()).await?;

        let mut fmt = rfb::PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 255.into(),
            green_max: 255.into(),
            blue_max: 255.into(),
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            padding: [0; 3],
        };

        let name = self.name.as_bytes();
        let (mut width, mut height) = self.fb.resolution();
        socket
            .write_all(
                rfb::ServerInit {
                    framebuffer_width: width.into(),
                    framebuffer_height: height.into(),
                    server_pixel_format: fmt,
                    name_length: (name.len() as u32).into(),
                }
                .as_bytes(),
            )
            .await?;
        socket.write_all(name).await?;

        let mut ready_for_update = false;
        let mut force_full_update = true;
        let mut send_cursor = false;
        let mut cur_fb: Vec<u32> = Vec::new();
        let mut tile_buf: Vec<u8> = Vec::new();
        let mut dirty_rects: Vec<(u16, u16, u16, u16)> = Vec::new();
        let mut zlib_buf: Vec<u8> = Vec::new();
        let mut zlib_stream = flate2::Compress::new(Compression::fast(), true);
        let mut scancode_state = scancode::State::new();
        loop {
            let mut socket_ready = false;
            let mut update_ready = false;
            let mut message_type = 0u8;
            let update_recv = &mut self.update_recv;
            let mut update: OptionFuture<_> = ready_for_update
                .then(|| update_recv.select_next_some())
                .into();
            futures::select! { // merge semantics
                _ = update => update_ready = true,
                r = socket.read(message_type.as_mut_bytes()).fuse() => {
                    if r? == 0 {
                        return Ok(())
                    }
                    socket_ready = true;
                }
            }

            if ready_for_update && update_ready {
                // Ensure the desktop size has not changed.
                let (new_width, new_height) = self.fb.resolution();
                if new_width != width || new_height != height {
                    if !self.supports_desktop_resize {
                        return Err(Error::ResizeUnsupported);
                    }
                    width = new_width;
                    height = new_height;
                    force_full_update = true;
                    // Notify the client of the new desktop size.
                    socket
                        .write_all(
                            rfb::FramebufferUpdate {
                                message_type: rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE,
                                padding: 0,
                                rectangle_count: 1.into(),
                            }
                            .as_bytes(),
                        )
                        .await?;
                    socket
                        .write_all(
                            rfb::Rectangle {
                                x: 0.into(),
                                y: 0.into(),
                                width: width.into(),
                                height: height.into(),
                                encoding_type: rfb::ENCODING_TYPE_DESKTOP_SIZE.into(),
                            }
                            .as_bytes(),
                        )
                        .await?;
                }

                // Read current framebuffer.
                let fb_size = width as usize * height as usize;
                cur_fb.resize(fb_size, 0);
                for y in 0..height {
                    let offset = y as usize * width as usize;
                    self.fb
                        .read_line(y, cur_fb[offset..offset + width as usize].as_mut_bytes());
                }

                let full_update = force_full_update || self.prev_fb.len() != fb_size;

                // Find dirty tiles.
                dirty_rects.clear();
                let mut ty: u16 = 0;
                while ty < height {
                    let tile_h = TILE_SIZE.min(height - ty);
                    let mut tx: u16 = 0;
                    while tx < width {
                        let tile_w = TILE_SIZE.min(width - tx);
                        let dirty = if full_update {
                            true
                        } else {
                            let mut d = false;
                            for y in ty..ty + tile_h {
                                let start = y as usize * width as usize + tx as usize;
                                if cur_fb[start..start + tile_w as usize]
                                    != self.prev_fb[start..start + tile_w as usize]
                                {
                                    d = true;
                                    break;
                                }
                            }
                            d
                        };
                        if dirty {
                            dirty_rects.push((tx, ty, tile_w, tile_h));
                        }
                        tx += TILE_SIZE;
                    }
                    ty += TILE_SIZE;
                }

                if !dirty_rects.is_empty() || send_cursor {
                    if !dirty_rects.is_empty() {
                        ready_for_update = false;
                    }
                    force_full_update = false;

                    let extra_rects = if send_cursor { 1u16 } else { 0 };

                    // Send FramebufferUpdate with dirty tiles + optional cursor.
                    socket
                        .write_all(
                            rfb::FramebufferUpdate {
                                message_type: rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE,
                                padding: 0,
                                rectangle_count: (dirty_rects.len() as u16 + extra_rects).into(),
                            }
                            .as_bytes(),
                        )
                        .await?;

                    if send_cursor {
                        send_cursor = false;
                        // 18x18 arrow cursor with white fill and 2px black outline.
                        // Each row is 18 pixels wide = 3 bytes in the bitmask.
                        #[rustfmt::skip]
                        const MASK: [[u8; 3]; 18] = [
                            [0b11000000, 0b00000000, 0b00000000],
                            [0b11100000, 0b00000000, 0b00000000],
                            [0b11110000, 0b00000000, 0b00000000],
                            [0b11111000, 0b00000000, 0b00000000],
                            [0b11111100, 0b00000000, 0b00000000],
                            [0b11111110, 0b00000000, 0b00000000],
                            [0b11111111, 0b00000000, 0b00000000],
                            [0b11111111, 0b10000000, 0b00000000],
                            [0b11111111, 0b11000000, 0b00000000],
                            [0b11111111, 0b11100000, 0b00000000],
                            [0b11111111, 0b11110000, 0b00000000],
                            [0b11111111, 0b00000000, 0b00000000],
                            [0b11111111, 0b00000000, 0b00000000],
                            [0b11100111, 0b10000000, 0b00000000],
                            [0b11000111, 0b10000000, 0b00000000],
                            [0b10000011, 0b11000000, 0b00000000],
                            [0b00000011, 0b11000000, 0b00000000],
                            [0b00000001, 0b10000000, 0b00000000],
                        ];
                        // Inner fill (white): 1 = white, 0 = black border
                        #[rustfmt::skip]
                        const FILL: [[u8; 3]; 18] = [
                            [0b00000000, 0b00000000, 0b00000000],
                            [0b00000000, 0b00000000, 0b00000000],
                            [0b01100000, 0b00000000, 0b00000000],
                            [0b01110000, 0b00000000, 0b00000000],
                            [0b01111000, 0b00000000, 0b00000000],
                            [0b01111100, 0b00000000, 0b00000000],
                            [0b01111110, 0b00000000, 0b00000000],
                            [0b01111111, 0b00000000, 0b00000000],
                            [0b01111111, 0b10000000, 0b00000000],
                            [0b01111111, 0b11000000, 0b00000000],
                            [0b01111100, 0b00000000, 0b00000000],
                            [0b01111100, 0b00000000, 0b00000000],
                            [0b01100110, 0b00000000, 0b00000000],
                            [0b00000011, 0b00000000, 0b00000000],
                            [0b00000011, 0b00000000, 0b00000000],
                            [0b00000001, 0b10000000, 0b00000000],
                            [0b00000001, 0b10000000, 0b00000000],
                            [0b00000000, 0b00000000, 0b00000000],
                        ];
                        let cw: u16 = 18;
                        let ch: u16 = 18;
                        let mask_stride = (cw as usize).div_ceil(8);
                        // Build cursor as 0x00RRGGBB u32 pixels, then convert
                        // through the negotiated pixel format.
                        const WHITE: u32 = 0x00FFFFFF;
                        const BLACK: u32 = 0x00000000;
                        let mut cursor_src = Vec::with_capacity(cw as usize * ch as usize);
                        for y in 0..ch as usize {
                            for x in 0..cw as usize {
                                let byte_i = x / 8;
                                let bit = 7 - (x % 8);
                                let in_mask =
                                    byte_i < mask_stride && (MASK[y][byte_i] >> bit) & 1 == 1;
                                let in_fill =
                                    byte_i < mask_stride && (FILL[y][byte_i] >> bit) & 1 == 1;
                                cursor_src.push(if in_mask && in_fill { WHITE } else { BLACK });
                            }
                        }
                        let mut pixels = Vec::new();
                        convert_pixels(&cursor_src, &fmt, &mut pixels);
                        let mask_flat: Vec<u8> =
                            MASK.iter().flat_map(|r| r.iter().copied()).collect();
                        socket
                            .write_all(
                                rfb::Rectangle {
                                    x: 0.into(),
                                    y: 0.into(),
                                    width: cw.into(),
                                    height: ch.into(),
                                    encoding_type: rfb::ENCODING_TYPE_CURSOR.into(),
                                }
                                .as_bytes(),
                            )
                            .await?;
                        socket.write_all(&pixels).await?;
                        socket.write_all(&mask_flat).await?;
                    }

                    let use_zlib = self.supports_zlib;

                    for &(tx, ty, tw, th) in &dirty_rects {
                        // Convert tile pixels.
                        tile_buf.clear();
                        for y in ty..ty + th {
                            let start = y as usize * width as usize + tx as usize;
                            convert_pixels(
                                &cur_fb[start..start + tw as usize],
                                &fmt,
                                &mut tile_buf,
                            );
                        }

                        if use_zlib {
                            // Compress tile data with zlib. The RFB spec
                            // requires a single continuous zlib stream per
                            // connection — use Sync flush to emit all pending
                            // output while preserving the dictionary.
                            zlib_buf.clear();
                            // Reserve enough space: worst case zlib output is
                            // slightly larger than input + overhead.
                            zlib_buf.resize(tile_buf.len() + 64, 0);
                            let before_in = zlib_stream.total_in();
                            let before_out = zlib_stream.total_out();
                            loop {
                                let status = zlib_stream
                                    .compress(
                                        &tile_buf[(zlib_stream.total_in() - before_in) as usize..],
                                        &mut zlib_buf
                                            [(zlib_stream.total_out() - before_out) as usize..],
                                        FlushCompress::Sync,
                                    )
                                    .map_err(|_| Error::ZlibCompression)?;
                                // Grow output buffer if needed.
                                let out_used = (zlib_stream.total_out() - before_out) as usize;
                                if out_used >= zlib_buf.len() - 16 {
                                    zlib_buf.resize(zlib_buf.len() * 2, 0);
                                }
                                let in_done =
                                    (zlib_stream.total_in() - before_in) as usize >= tile_buf.len();
                                if in_done && status == flate2::Status::Ok {
                                    break;
                                }
                            }
                            let compressed_len = (zlib_stream.total_out() - before_out) as usize;
                            zlib_buf.truncate(compressed_len);

                            socket
                                .write_all(
                                    rfb::Rectangle {
                                        x: tx.into(),
                                        y: ty.into(),
                                        width: tw.into(),
                                        height: th.into(),
                                        encoding_type: rfb::ENCODING_TYPE_ZLIB.into(),
                                    }
                                    .as_bytes(),
                                )
                                .await?;
                            // Zlib encoding: 4-byte length prefix + compressed data.
                            socket
                                .write_all(&(zlib_buf.len() as u32).to_be_bytes())
                                .await?;
                            socket.write_all(&zlib_buf).await?;
                        } else {
                            socket
                                .write_all(
                                    rfb::Rectangle {
                                        x: tx.into(),
                                        y: ty.into(),
                                        width: tw.into(),
                                        height: th.into(),
                                        encoding_type: rfb::ENCODING_TYPE_RAW.into(),
                                    }
                                    .as_bytes(),
                                )
                                .await?;
                            socket.write_all(&tile_buf).await?;
                        }
                    }
                }
                // else: nothing dirty, keep ready_for_update = true
                // so we check again on the next timer tick.

                std::mem::swap(&mut self.prev_fb, &mut cur_fb);
            }

            if socket_ready {
                match message_type {
                    rfb::CS_MESSAGE_SET_PIXEL_FORMAT => {
                        let mut input = rfb::SetPixelFormat::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        // Validate pixel format: only true-color with
                        // spec-defined bpp, and shifts must be < 32 to
                        // avoid panics in convert_pixels.
                        let pf = &input.pixel_format;
                        match pf.bits_per_pixel {
                            8 | 16 | 32 => {}
                            bpp => return Err(Error::UnsupportedPixelFormat(bpp)),
                        }
                        if pf.true_color_flag == 0
                            || pf.red_shift >= 32
                            || pf.green_shift >= 32
                            || pf.blue_shift >= 32
                        {
                            return Err(Error::UnsupportedPixelFormat(pf.bits_per_pixel));
                        }
                        fmt = input.pixel_format;
                        // Pixel format changed, force a full update.
                        force_full_update = true;
                    }
                    rfb::CS_MESSAGE_SET_ENCODINGS => {
                        let mut input = rfb::SetEncodings::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        let mut encodings: Vec<zerocopy::U32<zerocopy::BE>> =
                            vec![0.into(); input.encoding_count.get().into()];
                        socket.read_exact(encodings.as_mut_bytes()).await?;
                        self.supports_desktop_resize =
                            encodings.contains(&rfb::ENCODING_TYPE_DESKTOP_SIZE.into());
                        self.supports_zlib = encodings.contains(&rfb::ENCODING_TYPE_ZLIB.into());
                        let had_cursor = self.supports_cursor;
                        self.supports_cursor =
                            encodings.contains(&rfb::ENCODING_TYPE_CURSOR.into());
                        if self.supports_cursor && !had_cursor {
                            send_cursor = true;
                        }

                        if encodings.contains(&rfb::ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT.into()) {
                            // Request qemu extended key events.
                            let mut msg = rfb::FramebufferUpdate {
                                message_type: rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE,
                                padding: 0,
                                rectangle_count: 1.into(),
                            }
                            .as_bytes()
                            .to_vec();
                            msg.extend_from_slice(
                                rfb::Rectangle {
                                    x: 0.into(),
                                    y: 0.into(),
                                    width: 0.into(),
                                    height: 0.into(),
                                    encoding_type: rfb::ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT
                                        .into(),
                                }
                                .as_bytes(),
                            );
                            socket.write_all(&msg).await?;
                        }
                    }
                    rfb::CS_MESSAGE_FRAMEBUFFER_UPDATE_REQUEST => {
                        let mut input = rfb::FramebufferUpdateRequest::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        ready_for_update = true;
                        if input.incremental == 0 {
                            force_full_update = true;
                        }
                    }
                    rfb::CS_MESSAGE_KEY_EVENT => {
                        let mut input = rfb::KeyEvent::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;

                        // RFB key events are in xkeysym format. Convert them to
                        // US keyboard scancodes and send them to the keyboard
                        // device.
                        //
                        // Ideally the VNC client would support the qemu
                        // extensions that provide the scancodes directly.

                        // intercept ctrl-alt-p to paste clipboard contents
                        const KEYSYM_CONTROL_LEFT: u16 = 0xffe3;
                        const KEYSYM_ALT_LEFT: u16 = 0xffe9;

                        match input.key.get() as u16 {
                            KEYSYM_CONTROL_LEFT => self.ctrl_left_pressed = input.down_flag == 1,
                            KEYSYM_ALT_LEFT => self.alt_left_pressed = input.down_flag == 1,
                            _ => {}
                        }

                        if self.ctrl_left_pressed
                            && self.alt_left_pressed
                            && input.key.get() == b'p'.into()
                            && input.down_flag == 1
                        {
                            // release held modifier keys
                            self.ctrl_left_pressed = false;
                            self.alt_left_pressed = false;
                            for &scancode in &[KEYSYM_CONTROL_LEFT, KEYSYM_ALT_LEFT] {
                                let i = &mut self.input;
                                scancode_state.emit(scancode, false, |scancode, down| {
                                    i.key(scancode, down);
                                });
                            }

                            // make sure that the clipboard only contains printable ASCII chars
                            if self.clipboard.chars().all(|c| (' '..='~').contains(&c)) {
                                for c in self.clipboard.as_bytes() {
                                    let i = &mut self.input;
                                    scancode_state.emit_ascii_char(*c, true, |scancode, down| {
                                        i.key(scancode, down);
                                    });
                                    scancode_state.emit_ascii_char(*c, false, |scancode, down| {
                                        i.key(scancode, down);
                                    });
                                }
                            }
                        } else {
                            let i = &mut self.input;
                            scancode_state.emit(
                                input.key.get() as u16,
                                input.down_flag != 0,
                                |scancode, down| {
                                    i.key(scancode, down);
                                },
                            );
                        }
                    }
                    rfb::CS_MESSAGE_POINTER_EVENT => {
                        let mut input = rfb::PointerEvent::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        //scale the mouse coordinates in the VNC itself
                        let mut x = 0;
                        let mut y = 0;
                        //only absolute positioning is required; relative is not
                        if (width > 1) && (height > 1) {
                            let mut x_val = input.x.get() as u32;
                            let mut y_val = input.y.get() as u32;
                            if x_val > width as u32 - 1 {
                                x_val = width as u32 - 1;
                            }
                            if y_val > height as u32 - 1 {
                                y_val = height as u32 - 1;
                            }
                            x = ((x_val * HID_MOUSE_MAX_ABS_VALUE) / (width as u32 - 1)) as u16;
                            y = ((y_val * HID_MOUSE_MAX_ABS_VALUE) / (height as u32 - 1)) as u16;
                        }
                        self.input.mouse(input.button_mask, x, y);
                    }
                    rfb::CS_MESSAGE_CLIENT_CUT_TEXT => {
                        let mut input = rfb::ClientCutText::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        let mut text_latin1 = vec![0; input.length.get() as usize];
                        socket.read_exact(&mut text_latin1).await?;
                        // Latin1 characters map to the first 256 characters of Unicode (roughly).
                        self.clipboard = text_latin1.iter().copied().map(|c| c as char).collect();
                    }
                    rfb::CS_MESSAGE_QEMU => {
                        let mut input = rfb::QemuMessageHeader::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        match input.submessage_type {
                            rfb::QEMU_MESSAGE_EXTENDED_KEY_EVENT => {
                                let mut input = rfb::QemuExtendedKeyEvent::new_zeroed();
                                socket.read_exact(&mut input.as_mut_bytes()[2..]).await?;
                                let mut scancode = input.keycode.get() as u16;
                                // An E0 prefix is sometimes encoded via the
                                // high bit on a single byte.
                                if scancode & 0xff80 == 0x80 {
                                    scancode = 0xe000 | (scancode & 0x7f);
                                }
                                self.input.key(scancode, input.down_flag.get() != 0);
                            }
                            n => return Err(Error::UnknownQemuMessage(n)),
                        }
                    }
                    n => return Err(Error::UnknownMessage(n)),
                }
            }
        }
    }
}
