// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VNC server implementation.

mod rfb;
mod scancode;
use futures::channel::mpsc;
use futures::future::OptionFuture;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use pal_async::socket::PolledSocket;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported protocol version")]
    UnsupportedVersion(rfb::ProtocolVersion),
    #[error("unsupported message type: {0:#x}")]
    UnknownMessage(u8),
    #[error("unsupported qemu message type: {0:#x}")]
    UnknownQemuMessage(u8),
    #[error("socket error")]
    Io(#[from] std::io::Error),
    #[error("client does not support desktop resize extension")]
    DesktopResizeNotSupported,
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

impl<F: Framebuffer, I: Input> Server<F, I> {
    pub fn new(
        name: String,
        socket: PolledSocket<socket2::Socket>,
        fb: F,
        input: I,
    ) -> Server<F, I> {
        #[allow(clippy::disallowed_methods)] // TODO
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
            .write_all(rfb::ProtocolVersion(rfb::PROTOCOL_VERSION_33).as_bytes())
            .await?;

        let mut version = rfb::ProtocolVersion::new_zeroed();
        socket.read_exact(version.as_mut_bytes()).await?;

        if version.0 != rfb::PROTOCOL_VERSION_33 {
            return Err(Error::UnsupportedVersion(version));
        }

        socket
            .write_all(
                rfb::Security33 {
                    padding: [0; 3],
                    security_type: rfb::SECURITY_TYPE_NONE,
                }
                .as_bytes(),
            )
            .await?;

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
                ready_for_update = false;

                // Ensure the desktop size has not changed.
                let (new_width, new_height) = self.fb.resolution();
                if new_width != width || new_height != height {
                    // Send the new desktop size.
                    width = new_width;
                    height = new_height;
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
                } else {
                    // Send the update. Just update the whole framebuffer for now.
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
                                encoding_type: rfb::ENCODING_TYPE_RAW.into(),
                            }
                            .as_bytes(),
                        )
                        .await?;
                    let mut src_line = vec![0u32; width as usize];
                    let dest_depth = fmt.bits_per_pixel as usize / 8;
                    let shift_r = 24 - fmt.red_max.get().count_ones();
                    let shift_g = 16 - fmt.green_max.get().count_ones();
                    let shift_b = 8 - fmt.red_max.get().count_ones();
                    match dest_depth {
                        1 => {
                            let mut line = vec![0u8; width as usize];
                            for y in 0..height {
                                self.fb.read_line(y, src_line.as_mut_bytes());
                                for x in 0..width as usize {
                                    let p = src_line[x];
                                    let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
                                    let p2 = r >> shift_r << fmt.red_shift
                                        | g >> shift_g << fmt.green_shift
                                        | b >> shift_b << fmt.blue_shift;
                                    line[x] = p2 as u8;
                                }
                                socket.write_all(&line).await?;
                            }
                        }
                        2 => {
                            let mut line = vec![0u16; width as usize];
                            for y in 0..height {
                                self.fb.read_line(y, src_line.as_mut_bytes());
                                for x in 0..width as usize {
                                    let p = src_line[x];
                                    let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
                                    let p2 = r >> shift_r << fmt.red_shift
                                        | g >> shift_g << fmt.green_shift
                                        | b >> shift_b << fmt.blue_shift;
                                    line[x] = p2 as u16;
                                }
                                socket.write_all(line.as_bytes()).await?;
                            }
                        }
                        4 if shift_r == fmt.red_shift as u32
                            && shift_g == fmt.green_shift as u32
                            && shift_b == fmt.blue_shift as u32 =>
                        {
                            for y in 0..height {
                                self.fb.read_line(y, src_line.as_mut_bytes());
                                socket.write_all(src_line.as_bytes()).await?;
                            }
                        }
                        4 => {
                            let mut line = vec![0u32; width as usize];
                            for y in 0..height {
                                self.fb.read_line(y, src_line.as_mut_bytes());
                                for x in (width as usize & !3)..width as usize {
                                    let p = src_line[x];
                                    let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
                                    let p2 = r >> shift_r << fmt.red_shift
                                        | g >> shift_g << fmt.green_shift
                                        | b >> shift_b << fmt.blue_shift;
                                    line[x] = p2;
                                }
                                socket.write_all(line.as_bytes()).await?;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }

            if socket_ready {
                match message_type {
                    rfb::CS_MESSAGE_SET_PIXEL_FORMAT => {
                        let mut input = rfb::SetPixelFormat::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        fmt = input.pixel_format;
                    }
                    rfb::CS_MESSAGE_SET_ENCODINGS => {
                        let mut input = rfb::SetEncodings::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                        let mut encodings: Vec<zerocopy::U32<zerocopy::BE>> =
                            vec![0.into(); input.encoding_count.get().into()];
                        socket.read_exact(encodings.as_mut_bytes()).await?;
                        if !encodings.contains(&rfb::ENCODING_TYPE_DESKTOP_SIZE.into()) {
                            // Can't really operate without being able to change the desktop size dynamically.
                            return Err(Error::DesktopResizeNotSupported);
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
