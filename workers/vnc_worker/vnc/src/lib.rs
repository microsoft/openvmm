// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VNC server implementation for the RFB (Remote Framebuffer) protocol.
//!
//! Handles a single client connection: RFB handshake (version, security,
//! pixel format), framebuffer update encoding (raw or zlib), and client
//! input (keyboard, mouse, clipboard). Multiple concurrent connections
//! are managed by the worker layer, not this module.
//!
//! # Components
//!
//! - [`Encoder`]: per-connection zlib state and pixel format conversion.
//! - [`UpdateState`]: framebuffer snapshots and dirty detection (device rects
//!   or tile diff fallback).
//! - [`ClientState`]: per-connection mutable state (format, resolution, flags).
//! - [`Server`]: ties the above together with the RFB protocol state machine.
//!
//! # Keyboard Handling
//!
//! Two input paths: (1) QEMU extended key events send raw scancodes — the
//! guest layout maps them to characters. (2) Standard keysym events are
//! converted to US scancodes via `scancode.rs` (ASCII 32-126 only; non-ASCII
//! keysyms are dropped).
//!
//! Clipboard paste (Ctrl+Alt+P) types text into the guest. ASCII chars use
//! keysym→scancode; non-ASCII Latin-1 chars (öäü etc.) use the Windows
//! Alt+0+Numpad method (CP-1252). TigerVNC intercepts Ctrl+Alt, so paste
//! only works in RealVNC and noVNC.
//!
//! See `Guide/src/reference/devices/vnc/keyboard.md` for full details.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod dirty_bitmap;
mod rfb;
mod scancode;
pub use dirty_bitmap::DirtyBitmap;
pub use dirty_bitmap::Rect;
use flate2::Compression;
use flate2::FlushCompress;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::OptionFuture;
use pal_async::socket::PolledSocket;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const TILE_SIZE: u16 = dirty_bitmap::TILE_SIZE;

/// Receiver type for device-reported dirty rectangles. Arc-wrapped to avoid
/// cloning the Vec during per-client broadcast.
pub type DirtyRectReceiver = mpsc::Receiver<Arc<Vec<video_core::DirtyRect>>>;

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
    ZlibCompression(#[from] flate2::CompressError),
    #[error("socket error")]
    Io(#[from] std::io::Error),
}

/// A trait used to retrieve data from a framebuffer.
pub trait Framebuffer: Send + Sync {
    fn resolution(&mut self) -> (u16, u16);
    fn read_line(&mut self, line: u16, data: &mut [u8]);
}

const HID_MOUSE_MAX_ABS_VALUE: u32 = 0x7FFF;

/// A trait used to handle VNC client input.
pub trait Input {
    fn key(&mut self, scancode: u16, is_down: bool);
    fn mouse(&mut self, button_mask: u8, x: u16, y: u16);
}

// ---------------------------------------------------------------------------
// Encoder: handles pixel format conversion, zlib compression, and writing
// encoded rectangles to the socket.
// ---------------------------------------------------------------------------

/// Pre-computed pixel conversion parameters, cached per-connection to avoid
/// recomputing from the PixelFormat on every `convert_pixels` call.
#[derive(Clone, Copy)]
struct PixelConversion {
    dest_depth: usize,
    shift_r: u32,
    shift_g: u32,
    shift_b: u32,
    out_shift_r: u8,
    out_shift_g: u8,
    out_shift_b: u8,
    big_endian: bool,
    /// True when the client's format matches our internal 0x00RRGGBB layout
    /// and we can emit pixels as-is without per-pixel conversion.
    no_convert: bool,
}

impl PixelConversion {
    fn from_format(fmt: &rfb::PixelFormat) -> Self {
        let dest_depth = fmt.bits_per_pixel as usize / 8;
        // Use leading_zeros to derive bit width, not count_ones.
        // count_ones gives wrong results for non-conforming max values
        // (e.g., max=5 → count_ones=2, but actual width is 3).
        // leading_zeros on a u16 gives 16 - bit_width, so bit_width = 16 - lz.
        // Guard against max=0 (would produce bit_width=0, shift underflow).
        let red_bits = if fmt.red_max.get() > 0 {
            16 - fmt.red_max.get().leading_zeros()
        } else {
            8
        };
        let green_bits = if fmt.green_max.get() > 0 {
            16 - fmt.green_max.get().leading_zeros()
        } else {
            8
        };
        let blue_bits = if fmt.blue_max.get() > 0 {
            16 - fmt.blue_max.get().leading_zeros()
        } else {
            8
        };
        // Shift to align each channel from the internal 0x00RRGGBB layout
        // (R at bits 23..16, G at 15..8, B at 7..0) down to the client's
        // bit width before placing at the client's shift position.
        let shift_r = 24 - red_bits;
        let shift_g = 16 - green_bits;
        let shift_b = 8 - blue_bits;
        let big_endian = fmt.big_endian_flag != 0;
        let no_convert = dest_depth == 4
            && !big_endian
            && shift_r == fmt.red_shift as u32
            && shift_g == fmt.green_shift as u32
            && shift_b == fmt.blue_shift as u32;
        Self {
            dest_depth,
            shift_r,
            shift_g,
            shift_b,
            out_shift_r: fmt.red_shift,
            out_shift_g: fmt.green_shift,
            out_shift_b: fmt.blue_shift,
            big_endian,
            no_convert,
        }
    }
}

/// Convert source pixels (0x00RRGGBB layout) to the client's negotiated
/// pixel format and append the result to `out`. Uses pre-computed conversion
/// params to avoid recomputing shifts on every call.
fn convert_pixels(src: &[u32], pc: &PixelConversion, out: &mut Vec<u8>) {
    if pc.no_convert {
        out.extend_from_slice(src.as_bytes());
        return;
    }

    for &p in src {
        let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
        let p2 = r >> pc.shift_r << pc.out_shift_r
            | g >> pc.shift_g << pc.out_shift_g
            | b >> pc.shift_b << pc.out_shift_b;
        match (pc.dest_depth, pc.big_endian) {
            (1, _) => out.push(p2 as u8),
            (2, false) => out.extend_from_slice(&(p2 as u16).to_le_bytes()),
            (2, true) => out.extend_from_slice(&(p2 as u16).to_be_bytes()),
            (4, false) => out.extend_from_slice(&p2.to_le_bytes()),
            (4, true) => out.extend_from_slice(&p2.to_be_bytes()),
            _ => unreachable!(),
        }
    }
}

/// Manages per-connection zlib state and scratch buffers for encoding
/// framebuffer rectangles.
struct Encoder {
    tile_buf: Vec<u8>,
    zlib_buf: Vec<u8>,
    /// Accumulates the entire FramebufferUpdate message before sending,
    /// reducing multiple socket writes to a single write_all.
    output_buf: Vec<u8>,
    /// RFB requires a single continuous zlib stream per connection.
    zlib_stream: flate2::Compress,
}

impl Encoder {
    fn new() -> Self {
        Self {
            tile_buf: Vec::new(),
            zlib_buf: Vec::new(),
            output_buf: Vec::new(),
            zlib_stream: flate2::Compress::new(Compression::fast(), true),
        }
    }

    /// Encode a single rectangle into the output buffer (no socket write).
    /// `fb_width` is the framebuffer stride (pixels per scanline), needed
    /// to index into the linear `cur_fb` buffer.
    fn encode_rect(
        &mut self,
        cur_fb: &[u32],
        fb_width: u16,
        pc: &PixelConversion,
        rect: &Rect,
        use_zlib: bool,
    ) -> Result<usize, Error> {
        // Pre-allocate tile_buf to avoid reallocation in the scanline loop.
        self.tile_buf.clear();
        self.tile_buf
            .reserve(rect.w as usize * rect.h as usize * pc.dest_depth);

        // Hoist the no-convert check out of the per-scanline loop.
        // For the common 32bpp-native case, this avoids a function call
        // + branch per scanline.
        if pc.no_convert {
            for y in rect.y..rect.y + rect.h {
                let start = y as usize * fb_width as usize + rect.x as usize;
                self.tile_buf
                    .extend_from_slice(cur_fb[start..start + rect.w as usize].as_bytes());
            }
        } else {
            for y in rect.y..rect.y + rect.h {
                let start = y as usize * fb_width as usize + rect.x as usize;
                convert_pixels(
                    &cur_fb[start..start + rect.w as usize],
                    pc,
                    &mut self.tile_buf,
                );
            }
        }

        if use_zlib {
            self.append_zlib(rect)
        } else {
            self.append_raw(rect)
        }
    }

    /// Compress tile_buf with zlib and append to output_buf.
    fn append_zlib(&mut self, rect: &Rect) -> Result<usize, Error> {
        // Compressed output is almost always smaller than input. Allocate
        // input size + margin for zlib overhead and Sync flush trailer.
        // The Vec retains capacity across calls, so after the first large
        // rect this allocation is typically a no-op.
        let initial_capacity = self.tile_buf.len() + 128;
        self.zlib_buf.clear();
        self.zlib_buf.resize(initial_capacity, 0);

        let before_in = self.zlib_stream.total_in();
        let before_out = self.zlib_stream.total_out();
        loop {
            let in_offset = (self.zlib_stream.total_in() - before_in) as usize;
            let out_offset = (self.zlib_stream.total_out() - before_out) as usize;
            let status = self.zlib_stream.compress(
                &self.tile_buf[in_offset..],
                &mut self.zlib_buf[out_offset..],
                FlushCompress::Sync,
            )?;
            let out_used = (self.zlib_stream.total_out() - before_out) as usize;
            let in_done = (self.zlib_stream.total_in() - before_in) as usize >= self.tile_buf.len();
            if in_done && status == flate2::Status::Ok {
                break;
            }
            // Rare: incompressible data exceeded buffer. Double and retry.
            if out_used >= self.zlib_buf.len() - 16 {
                self.zlib_buf.resize(self.zlib_buf.len() * 2, 0);
            }
        }
        let compressed_len = (self.zlib_stream.total_out() - before_out) as usize;
        self.zlib_buf.truncate(compressed_len);

        self.output_buf.extend_from_slice(
            rfb::Rectangle {
                x: rect.x.into(),
                y: rect.y.into(),
                width: rect.w.into(),
                height: rect.h.into(),
                encoding_type: rfb::ENCODING_TYPE_ZLIB.into(),
            }
            .as_bytes(),
        );
        self.output_buf
            .extend_from_slice(&(self.zlib_buf.len() as u32).to_be_bytes());
        self.output_buf.extend_from_slice(&self.zlib_buf);
        // rect header (12) + length prefix (4) + compressed data
        Ok(12 + 4 + self.zlib_buf.len())
    }

    /// Append tile_buf as raw (uncompressed) rect to output_buf.
    fn append_raw(&mut self, rect: &Rect) -> Result<usize, Error> {
        self.output_buf.extend_from_slice(
            rfb::Rectangle {
                x: rect.x.into(),
                y: rect.y.into(),
                width: rect.w.into(),
                height: rect.h.into(),
                encoding_type: rfb::ENCODING_TYPE_RAW.into(),
            }
            .as_bytes(),
        );
        self.output_buf.extend_from_slice(&self.tile_buf);
        // rect header (12) + raw pixel data
        Ok(12 + self.tile_buf.len())
    }
}

// ---------------------------------------------------------------------------
// Cursor: builds the software cursor update rectangle.
// ---------------------------------------------------------------------------

/// Build the default 18x18 arrow cursor as a VNC cursor pseudo-encoding.
/// Returns (pixel_data, mask_data) in the client's pixel format.
fn build_cursor(pc: &PixelConversion) -> (Vec<u8>, Vec<u8>) {
    // 18x18 arrow cursor with white fill and 2px black outline.
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

    const CW: usize = 18;
    const CH: usize = 18;
    const WHITE: u32 = 0x00FFFFFF;
    const BLACK: u32 = 0x00000000;
    let mask_stride = CW.div_ceil(8);

    let mut cursor_src = Vec::with_capacity(CW * CH);
    for y in 0..CH {
        for x in 0..CW {
            let byte_i = x / 8;
            let bit = 7 - (x % 8);
            let in_mask = byte_i < mask_stride && (MASK[y][byte_i] >> bit) & 1 == 1;
            let in_fill = byte_i < mask_stride && (FILL[y][byte_i] >> bit) & 1 == 1;
            cursor_src.push(if in_mask && in_fill { WHITE } else { BLACK });
        }
    }
    let mut pixels = Vec::new();
    convert_pixels(&cursor_src, pc, &mut pixels);
    let mask_flat: Vec<u8> = MASK.iter().flat_map(|r| r.iter().copied()).collect();
    (pixels, mask_flat)
}

// ---------------------------------------------------------------------------
// UpdateState: manages framebuffer snapshots and dirty detection.
// ---------------------------------------------------------------------------

/// How dirty regions were determined this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtySource {
    /// Full screen refresh (first frame, resolution change, client request).
    Full,
    /// Dirty rects provided by the guest video driver.
    Device,
    /// Tile-by-tile comparison against previous frame (fallback).
    Diff,
}

impl DirtySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Device => "device",
            Self::Diff => "diff",
        }
    }
}

/// Result of a dirty collection cycle.
struct DirtyResult {
    rects: Vec<Rect>,
    source: DirtySource,
}

/// Tracks framebuffer state for determining which regions need updating.
struct UpdateState {
    cur_fb: Vec<u32>,
    prev_fb: Vec<u32>,
    pending_dirty: DirtyBitmap,
    /// Reusable buffer for merge results, avoids allocation per cycle.
    merged_rects: Vec<Rect>,
    width: u16,
    height: u16,
    /// Set once device dirty rects have been received. When true, an empty
    /// dirty channel means "nothing changed" and we skip the expensive full
    /// VRAM read and tile diff that would otherwise run every 30ms.
    device_dirty_seen: bool,
}

impl UpdateState {
    fn new() -> Self {
        Self {
            cur_fb: Vec::new(),
            prev_fb: Vec::new(),
            pending_dirty: DirtyBitmap::new(0, 0),
            merged_rects: Vec::new(),
            width: 0,
            height: 0,
            device_dirty_seen: false,
        }
    }

    /// Update resolution tracking when the framebuffer size changes.
    fn set_resolution(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Read the framebuffer and determine which rectangles are dirty.
    /// Returns merged dirty rects in pixel coordinates and the source
    /// that provided them (full refresh, device, or tile diff).
    fn collect_dirty(
        &mut self,
        fb: &mut impl Framebuffer,
        dirty_recv: &mut Option<DirtyRectReceiver>,
        force_full: bool,
        missed_dirty: &Option<Arc<AtomicBool>>,
    ) -> DirtyResult {
        let (width, height) = (self.width, self.height);
        let fb_size = width as usize * height as usize;
        let mut full_update = force_full || self.prev_fb.len() != fb_size;

        if full_update {
            self.pending_dirty.resize(width, height);
        }

        // Drain any device-reported dirty rects into our pending bitmap.
        let mut got_device_dirty = false;
        if let Some(recv) = dirty_recv {
            loop {
                match recv.try_next() {
                    Ok(Some(rects)) => {
                        for r in rects.iter() {
                            self.pending_dirty
                                .mark_rect(r.left, r.top, r.right, r.bottom);
                        }
                        got_device_dirty = true;
                    }
                    Ok(None) => {
                        // Channel closed (upstream video device reset or
                        // coordinator dropped senders). Reset to tile diff
                        // and stop polling the dead channel.
                        if self.device_dirty_seen {
                            tracing::info!("dirty channel closed, falling back to tile diff");
                            self.device_dirty_seen = false;
                        }
                        *dirty_recv = None;
                        break;
                    }
                    Err(_) => break, // Empty, nothing pending
                }
            }
        }
        if got_device_dirty {
            self.device_dirty_seen = true;
        }

        // If the coordinator flagged that it dropped a dirty broadcast
        // because our channel was full, force a full refresh to prevent
        // permanently stale regions.
        if let Some(missed) = missed_dirty {
            if missed.swap(false, Ordering::Relaxed) {
                full_update = true;
                tracing::debug!("missed dirty broadcast, forcing full refresh");
            }
        }

        let source = if full_update {
            self.pending_dirty.mark_all();
            self.read_full_framebuffer(fb);
            self.pending_dirty.merge_into(&mut self.merged_rects);
            DirtySource::Full
        } else if got_device_dirty {
            // Merge once: reuse for both partial VRAM reads and final output.
            self.pending_dirty.merge_into(&mut self.merged_rects);
            // Swap prev_fb into cur_fb (O(1) pointer swap) so non-dirty
            // regions are already correct, then overwrite dirty lines.
            std::mem::swap(&mut self.cur_fb, &mut self.prev_fb);
            for r in &self.merged_rects {
                for y in r.y..r.y + r.h {
                    let offset = y as usize * width as usize;
                    fb.read_line(
                        y,
                        self.cur_fb[offset..offset + width as usize].as_mut_bytes(),
                    );
                }
            }
            DirtySource::Device
        } else if self.device_dirty_seen {
            // Device supports dirty rects but sent nothing this cycle --
            // nothing changed. Skip the 8MB VRAM read entirely.
            self.merged_rects.clear();
            DirtySource::Device
        } else {
            // No device dirty support: full read + tile diff (hyperv_fb fallback).
            self.read_full_framebuffer(fb);
            self.tile_diff();
            self.pending_dirty.merge_into(&mut self.merged_rects);
            DirtySource::Diff
        };

        self.pending_dirty.clear();
        // Swap out the merged rects so caller owns them. The empty Vec
        // we swap in will be reused by merge_into next cycle.
        let mut rects = Vec::new();
        std::mem::swap(&mut rects, &mut self.merged_rects);
        DirtyResult { rects, source }
    }

    /// Read the entire framebuffer into cur_fb.
    fn read_full_framebuffer(&mut self, fb: &mut impl Framebuffer) {
        let fb_size = self.width as usize * self.height as usize;
        self.cur_fb.resize(fb_size, 0);
        for y in 0..self.height {
            let offset = y as usize * self.width as usize;
            fb.read_line(
                y,
                self.cur_fb[offset..offset + self.width as usize].as_mut_bytes(),
            );
        }
    }

    /// Compare cur_fb against prev_fb tile-by-tile and mark changed tiles
    /// in pending_dirty.
    fn tile_diff(&mut self) {
        let (width, height) = (self.width, self.height);
        let mut ty: u16 = 0;
        while ty < height {
            let tile_h = TILE_SIZE.min(height - ty);
            let mut tx: u16 = 0;
            while tx < width {
                let tile_w = TILE_SIZE.min(width - tx);
                let mut changed = false;
                for y in ty..ty + tile_h {
                    let start = y as usize * width as usize + tx as usize;
                    if self.cur_fb[start..start + tile_w as usize]
                        != self.prev_fb[start..start + tile_w as usize]
                    {
                        changed = true;
                        break;
                    }
                }
                if changed {
                    // Use set_tile directly — we already know the tile coords,
                    // no need for mark_rect's clamping and division.
                    self.pending_dirty.set_tile(tx / TILE_SIZE, ty / TILE_SIZE);
                }
                tx += TILE_SIZE;
            }
            ty += TILE_SIZE;
        }
    }

    /// Return a used rects Vec so its allocation can be reused next cycle.
    fn recycle_rects(&mut self, rects: Vec<Rect>) {
        // Swap to preserve the Vec's allocation capacity for next cycle.
        self.merged_rects = rects;
        self.merged_rects.clear();
    }

    /// Swap cur_fb into prev_fb for next cycle's comparison baseline.
    fn commit(&mut self) {
        std::mem::swap(&mut self.prev_fb, &mut self.cur_fb);
    }
}

// ---------------------------------------------------------------------------
// Server: ties together protocol handling, update orchestration, and encoding.
// ---------------------------------------------------------------------------

/// Mutable per-connection state passed between the event loop and message
/// handler. Groups pixel format, resolution, encoding capabilities, and
/// update flags into a single struct to avoid long parameter lists.
struct ClientState {
    /// Client's negotiated pixel format (bpp, endianness, color shifts).
    fmt: rfb::PixelFormat,
    /// Pre-computed conversion params from fmt, cached to avoid recomputing
    /// per pixel in the hot encoding loop.
    pixel_conv: PixelConversion,
    /// Current framebuffer width in pixels.
    width: u16,
    /// Current framebuffer height in pixels.
    height: u16,
    /// True once the client has requested a framebuffer update.
    ready_for_update: bool,
    /// Forces a full screen refresh on the next update cycle.
    force_full_update: bool,
    /// Send cursor shape on the next update (set when client first
    /// advertises cursor encoding support).
    send_cursor: bool,
    /// Converts xkeysym key events to scancodes.
    scancode_state: scancode::State,
    /// Client supports the DesktopSize pseudo-encoding (resolution change).
    supports_desktop_resize: bool,
    /// Client supports zlib-compressed rectangles.
    supports_zlib: bool,
    /// Client supports the Cursor pseudo-encoding.
    supports_cursor: bool,
}

/// A VNC server handling a single connection.
pub struct Server<F, I> {
    socket: PolledSocket<socket2::Socket>,
    fb: F,
    input: I,
    update_recv: mpsc::Receiver<()>,
    update_send: mpsc::Sender<()>,
    name: String,

    /// Ctrl-Alt-P paste intercept: tracks modifier key state (left or right).
    ctrl_pressed: bool,
    alt_pressed: bool,
    /// Clipboard text received from the client via ClientCutText.
    clipboard: String,

    dirty_recv: Option<DirtyRectReceiver>,
    /// Set by the coordinator when a dirty broadcast was dropped because
    /// the per-client channel was full. The client checks and clears this
    /// flag during collect_dirty to force a full refresh.
    missed_dirty: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
pub struct Updater(mpsc::Sender<()>);

impl Updater {
    pub fn update(&mut self) {
        // Capacity-1 channel coalesces multiple signals into one.
        let _ = self.0.try_send(());
    }
}

impl<F: Framebuffer, I: Input> Server<F, I> {
    pub fn new(
        name: String,
        socket: PolledSocket<socket2::Socket>,
        fb: F,
        input: I,
        dirty_recv: Option<DirtyRectReceiver>,
        missed_dirty: Option<Arc<AtomicBool>>,
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

            ctrl_pressed: false,
            alt_pressed: false,
            clipboard: String::new(),

            dirty_recv,
            missed_dirty,
        }
    }

    pub fn updater(&mut self) -> Updater {
        Updater(self.update_send.clone())
    }

    pub fn done(self) -> (F, I) {
        (self.fb, self.input)
    }

    /// Runs the VNC server. Treats client disconnects as normal completion.
    pub async fn run(&mut self) -> Result<(), Error> {
        match self.run_internal().await {
            Ok(()) => Ok(()),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::ConnectionReset => Ok(()),
            err => err,
        }
    }

    // -----------------------------------------------------------------------
    // Handshake
    // -----------------------------------------------------------------------

    /// Perform the RFB protocol handshake: version, security, and init.
    /// Returns the initial (width, height, pixel_format).
    async fn handshake(&mut self) -> Result<(u16, u16, rfb::PixelFormat), Error> {
        let socket = &mut self.socket;
        socket
            .write_all(rfb::ProtocolVersion(rfb::PROTOCOL_VERSION_38).as_bytes())
            .await?;

        let mut version = rfb::ProtocolVersion::new_zeroed();
        socket.read_exact(version.as_mut_bytes()).await?;

        let version_str = std::str::from_utf8(&version.0).unwrap_or("unknown").trim();
        tracing::info!(version = version_str, "client RFB version");

        match version.0 {
            rfb::PROTOCOL_VERSION_33 => {
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
                socket
                    .write_all(rfb::Security37 { type_count: 1 }.as_bytes())
                    .await?;
                socket.write_all(&[rfb::SECURITY_TYPE_NONE]).await?;

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

        let fmt = rfb::PixelFormat {
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
        let (width, height) = self.fb.resolution();
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

        tracing::info!(
            width,
            height,
            bpp = fmt.bits_per_pixel,
            depth = fmt.depth,
            "initial framebuffer"
        );

        Ok((width, height, fmt))
    }

    // -----------------------------------------------------------------------
    // Client message handling
    // -----------------------------------------------------------------------

    /// Paste clipboard contents as keystrokes to the VM.
    ///
    /// ASCII (32-126): emitted via keysym → scancode mapping.
    /// Latin-1 (128-255, e.g., ö, ä, ü): emitted via Windows Alt+Numpad
    /// input method (hold Alt, type decimal codepoint on numpad with leading
    /// 0 for CP-1252, release Alt). Only works for Windows guests — Linux
    /// guests need XKB compose sequences which aren't supported yet.
    /// Characters beyond Latin-1 (U+0100+): silently skipped.
    fn paste_clipboard(&mut self, cs: &mut ClientState) {
        // Numpad scancodes for Alt+Numpad input method (digits 0-9).
        const NUMPAD_SC: [u16; 10] = [0x52, 0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
        const SC_ALT: u16 = 0x38;

        for c in self.clipboard.chars() {
            let codepoint = c as u32;

            if (32..=126).contains(&codepoint) {
                // ASCII: emit via keysym → scancode mapping.
                let keysym = codepoint as u16;
                let i = &mut self.input;
                cs.scancode_state.emit(keysym, true, |sc, down| {
                    i.key(sc, down);
                });
                let i = &mut self.input;
                cs.scancode_state.emit(keysym, false, |sc, down| {
                    i.key(sc, down);
                });
            } else if codepoint < 256 {
                // Latin-1: Windows Alt+Numpad with leading 0 for CP-1252.
                // Compute digits arithmetically — no allocation.
                // "0" prefix tells Windows to use CP-1252 instead of OEM codepage.
                let d2 = (codepoint / 100) as u8;
                let d1 = ((codepoint / 10) % 10) as u8;
                let d0 = (codepoint % 10) as u8;
                // Leading 0 + up to 3 digits (max codepoint is 255).
                let digits: &[u8] = if codepoint >= 100 {
                    &[0, d2, d1, d0]
                } else if codepoint >= 10 {
                    &[0, d1, d0]
                } else {
                    &[0, d0]
                };

                self.input.key(SC_ALT, true);
                for &d in digits {
                    self.input.key(NUMPAD_SC[d as usize], true);
                    self.input.key(NUMPAD_SC[d as usize], false);
                }
                self.input.key(SC_ALT, false);
            }
            // Characters beyond Latin-1 (U+0100+) are silently skipped.
        }
    }

    /// Process a single client-to-server message.
    async fn handle_client_message(
        &mut self,
        message_type: u8,
        cs: &mut ClientState,
    ) -> Result<(), Error> {
        let socket = &mut self.socket;
        match message_type {
            rfb::CS_MESSAGE_SET_PIXEL_FORMAT => {
                let mut input = rfb::SetPixelFormat::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                let pf = &input.pixel_format;
                match pf.bits_per_pixel {
                    8 | 16 | 32 => {}
                    bpp => return Err(Error::UnsupportedPixelFormat(bpp)),
                }
                // Reject formats where channels don't fit in bits_per_pixel.
                // A shift of 31 with an 8-bit channel would overflow u32
                // in the pixel conversion shift operations.
                let r_bits = if pf.red_max.get() > 0 {
                    16 - pf.red_max.get().leading_zeros()
                } else {
                    0
                };
                let g_bits = if pf.green_max.get() > 0 {
                    16 - pf.green_max.get().leading_zeros()
                } else {
                    0
                };
                let b_bits = if pf.blue_max.get() > 0 {
                    16 - pf.blue_max.get().leading_zeros()
                } else {
                    0
                };
                // Reject: non-true-color, channels exceeding our 8-bit
                // internal format (shift_x = 8 - bits would underflow),
                // or shift + bits overflowing u32.
                if pf.true_color_flag == 0
                    || r_bits > 8
                    || g_bits > 8
                    || b_bits > 8
                    || pf.red_shift as u32 + r_bits > 32
                    || pf.green_shift as u32 + g_bits > 32
                    || pf.blue_shift as u32 + b_bits > 32
                {
                    return Err(Error::UnsupportedPixelFormat(pf.bits_per_pixel));
                }
                // Warn on non-conforming max values. The RFB spec says max
                // "should" be 2^N - 1 but uses "should" not "must". We handle
                // any value via leading_zeros, but log unusual ones.
                for (name, max) in [
                    ("red", pf.red_max.get()),
                    ("green", pf.green_max.get()),
                    ("blue", pf.blue_max.get()),
                ] {
                    if max == 0 {
                        tracing::debug!(channel = name, "pixel format has zero max");
                    } else if max & (max + 1) != 0 {
                        // Not of the form 2^N - 1
                        tracing::debug!(channel = name, max, "pixel format max is not 2^N-1");
                    }
                }
                cs.fmt = input.pixel_format;
                cs.pixel_conv = PixelConversion::from_format(&cs.fmt);
                tracing::info!(
                    bpp = cs.fmt.bits_per_pixel,
                    depth = cs.fmt.depth,
                    big_endian = cs.fmt.big_endian_flag != 0,
                    red_shift = cs.fmt.red_shift,
                    green_shift = cs.fmt.green_shift,
                    blue_shift = cs.fmt.blue_shift,
                    "client pixel format changed"
                );
                cs.force_full_update = true;
            }
            rfb::CS_MESSAGE_SET_ENCODINGS => {
                let mut input = rfb::SetEncodings::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                // Cap allocation at 4096 to prevent OOM from malicious clients.
                // If the client advertises more, we still drain the full
                // message to keep the RFB stream in sync.
                let advertised = input.encoding_count.get() as usize;
                let capped = advertised.min(4096);
                let mut encodings: Vec<zerocopy::U32<zerocopy::BE>> = vec![0.into(); capped];
                socket.read_exact(encodings.as_mut_bytes()).await?;
                if advertised > capped {
                    // Drain the remaining entries we won't process.
                    let remaining = advertised - capped;
                    let mut discard: Vec<zerocopy::U32<zerocopy::BE>> =
                        vec![0.into(); remaining.min(4096)];
                    let mut left = remaining;
                    while left > 0 {
                        let chunk = left.min(4096);
                        discard.truncate(chunk);
                        discard.resize(chunk, 0.into());
                        socket.read_exact(discard.as_mut_bytes()).await?;
                        left -= chunk;
                    }
                }
                cs.supports_desktop_resize =
                    encodings.contains(&rfb::ENCODING_TYPE_DESKTOP_SIZE.into());
                cs.supports_zlib = encodings.contains(&rfb::ENCODING_TYPE_ZLIB.into());
                let had_cursor = cs.supports_cursor;
                cs.supports_cursor = encodings.contains(&rfb::ENCODING_TYPE_CURSOR.into());
                tracing::info!(
                    zlib = cs.supports_zlib,
                    desktop_resize = cs.supports_desktop_resize,
                    cursor = cs.supports_cursor,
                    encoding_count = encodings.len(),
                    "client encodings"
                );
                if cs.supports_cursor && !had_cursor {
                    cs.send_cursor = true;
                }

                if encodings.contains(&rfb::ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT.into()) {
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
                            encoding_type: rfb::ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT.into(),
                        }
                        .as_bytes(),
                    );
                    socket.write_all(&msg).await?;
                }
            }
            rfb::CS_MESSAGE_FRAMEBUFFER_UPDATE_REQUEST => {
                let mut input = rfb::FramebufferUpdateRequest::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                cs.ready_for_update = true;
                if input.incremental == 0 {
                    cs.force_full_update = true;
                }
            }
            rfb::CS_MESSAGE_KEY_EVENT => {
                let mut input = rfb::KeyEvent::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;

                // Track both left and right modifier keys. Use u32 to
                // match the full keysym without truncation.
                const KEYSYM_CONTROL_LEFT: u32 = 0xffe3;
                const KEYSYM_CONTROL_RIGHT: u32 = 0xffe4;
                const KEYSYM_ALT_LEFT: u32 = 0xffe9;
                const KEYSYM_ALT_RIGHT: u32 = 0xffea;

                let key = input.key.get();
                let is_down = input.down_flag == 1;

                tracing::trace!(keysym = key, down = is_down, "key event");

                match key {
                    KEYSYM_CONTROL_LEFT | KEYSYM_CONTROL_RIGHT => self.ctrl_pressed = is_down,
                    KEYSYM_ALT_LEFT | KEYSYM_ALT_RIGHT => self.alt_pressed = is_down,
                    _ => {}
                }

                if self.ctrl_pressed
                    && self.alt_pressed
                    && (key == u32::from(b'p') || key == u32::from(b'P'))
                    && is_down
                {
                    tracing::debug!(
                        clipboard_len = self.clipboard.len(),
                        "Ctrl-Alt-P paste triggered"
                    );
                    // Release modifiers before pasting.
                    self.ctrl_pressed = false;
                    self.alt_pressed = false;
                    for &keysym in &[
                        KEYSYM_CONTROL_LEFT as u16,
                        KEYSYM_CONTROL_RIGHT as u16,
                        KEYSYM_ALT_LEFT as u16,
                        KEYSYM_ALT_RIGHT as u16,
                    ] {
                        let i = &mut self.input;
                        cs.scancode_state.emit(keysym, false, |sc, down| {
                            i.key(sc, down);
                        });
                    }
                    self.paste_clipboard(cs);
                } else if key <= u32::from(u16::MAX) {
                    // Only emit keysyms that fit in u16 — the scancode table
                    // only has entries for standard X keysyms (0x0000-0xFFFF).
                    // High-plane Unicode keysyms (0x01000000+) have no scancode
                    // mapping and would be misinterpreted if truncated.
                    let i = &mut self.input;
                    cs.scancode_state
                        .emit(key as u16, input.down_flag != 0, |scancode, down| {
                            i.key(scancode, down);
                        });
                }
            }
            rfb::CS_MESSAGE_POINTER_EVENT => {
                let mut input = rfb::PointerEvent::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                let (mut x, mut y) = (0u16, 0u16);
                if cs.width > 1 && cs.height > 1 {
                    let x_val = (input.x.get() as u32).min(cs.width as u32 - 1);
                    let y_val = (input.y.get() as u32).min(cs.height as u32 - 1);
                    x = ((x_val * HID_MOUSE_MAX_ABS_VALUE) / (cs.width as u32 - 1)) as u16;
                    y = ((y_val * HID_MOUSE_MAX_ABS_VALUE) / (cs.height as u32 - 1)) as u16;
                }
                self.input.mouse(input.button_mask, x, y);
            }
            rfb::CS_MESSAGE_CLIENT_CUT_TEXT => {
                let mut input = rfb::ClientCutText::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                // Cap clipboard size to prevent allocation attacks.
                // Drain the full message to keep the stream in sync.
                let advertised = input.length.get() as usize;
                let len = advertised.min(1024 * 1024);
                let mut text_latin1 = vec![0; len];
                socket.read_exact(&mut text_latin1).await?;
                if advertised > len {
                    let mut left = advertised - len;
                    let mut discard = vec![0u8; left.min(4096)];
                    while left > 0 {
                        let chunk = left.min(discard.len());
                        socket.read_exact(&mut discard[..chunk]).await?;
                        left -= chunk;
                    }
                }
                self.clipboard = text_latin1.iter().copied().map(|c| c as char).collect();
                tracing::debug!(
                    clipboard_len = self.clipboard.len(),
                    "received ClientCutText"
                );
            }
            rfb::CS_MESSAGE_QEMU => {
                let mut input = rfb::QemuMessageHeader::new_zeroed();
                socket.read_exact(&mut input.as_mut_bytes()[1..]).await?;
                match input.submessage_type {
                    rfb::QEMU_MESSAGE_EXTENDED_KEY_EVENT => {
                        let mut input = rfb::QemuExtendedKeyEvent::new_zeroed();
                        socket.read_exact(&mut input.as_mut_bytes()[2..]).await?;
                        let mut scancode = input.keycode.get() as u16;
                        // An E0 prefix is sometimes encoded via the high bit
                        // on a single byte.
                        if scancode & 0xff80 == 0x80 {
                            scancode = 0xe000 | (scancode & 0x7f);
                        }
                        let is_down = input.down_flag.get() != 0;

                        tracing::trace!(
                            scancode = format_args!("{:#06x}", scancode),
                            down = is_down,
                            "qemu extended key event"
                        );

                        // Track both left and right modifier scancodes.
                        const SC_CTRL_LEFT: u16 = 0x1d;
                        const SC_CTRL_RIGHT: u16 = 0xe01d;
                        const SC_ALT_LEFT: u16 = 0x38;
                        const SC_ALT_RIGHT: u16 = 0xe038;
                        const SC_P: u16 = 0x19;

                        match scancode {
                            SC_CTRL_LEFT | SC_CTRL_RIGHT => self.ctrl_pressed = is_down,
                            SC_ALT_LEFT | SC_ALT_RIGHT => self.alt_pressed = is_down,
                            _ => {}
                        }

                        if self.ctrl_pressed && self.alt_pressed && scancode == SC_P && is_down {
                            tracing::debug!(
                                clipboard_len = self.clipboard.len(),
                                "Ctrl-Alt-P paste triggered (scancode)"
                            );
                            self.ctrl_pressed = false;
                            self.alt_pressed = false;
                            // Release both left and right modifiers via raw
                            // scancodes. The user may have triggered paste
                            // with right Ctrl/Alt.
                            self.input.key(SC_CTRL_LEFT, false);
                            self.input.key(SC_CTRL_RIGHT, false);
                            self.input.key(SC_ALT_LEFT, false);
                            self.input.key(SC_ALT_RIGHT, false);
                            self.paste_clipboard(cs);
                        } else {
                            // Forward raw scancodes. The scancode represents
                            // the physical key — the guest OS layout maps it
                            // to the correct character.
                            //
                            // RealVNC sends US-layout scancodes regardless of
                            // client keyboard — this is a RealVNC client bug
                            // that cannot be fixed server-side.
                            self.input.key(scancode, is_down);
                        }
                    }
                    n => return Err(Error::UnknownQemuMessage(n)),
                }
            }
            n => return Err(Error::UnknownMessage(n)),
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    async fn run_internal(&mut self) -> Result<(), Error> {
        let (width, height, fmt) = self.handshake().await?;

        let mut cs = ClientState {
            pixel_conv: PixelConversion::from_format(&fmt),
            fmt,
            width,
            height,
            ready_for_update: false,
            force_full_update: true,
            send_cursor: false,
            scancode_state: scancode::State::new(),
            supports_desktop_resize: false,
            supports_zlib: false,
            supports_cursor: false,
        };
        let mut encoder = Encoder::new();
        let mut update_state = UpdateState::new();

        update_state.set_resolution(cs.width, cs.height);

        loop {
            let mut socket_ready = false;
            let mut update_ready = false;
            let mut message_type = 0u8;
            let update_recv = &mut self.update_recv;
            let mut update: OptionFuture<_> = cs
                .ready_for_update
                .then(|| update_recv.select_next_some())
                .into();
            futures::select! { // merge semantics
                _ = update => update_ready = true,
                r = self.socket.read(message_type.as_mut_bytes()).fuse() => {
                    if r? == 0 {
                        return Ok(())
                    }
                    socket_ready = true;
                }
            }

            if cs.ready_for_update && update_ready {
                // Check for resolution change.
                let (new_width, new_height) = self.fb.resolution();
                if new_width != cs.width || new_height != cs.height {
                    if !cs.supports_desktop_resize {
                        return Err(Error::ResizeUnsupported);
                    }
                    tracing::info!(
                        old_width = cs.width,
                        old_height = cs.height,
                        new_width,
                        new_height,
                        "resolution changed"
                    );
                    cs.width = new_width;
                    cs.height = new_height;
                    update_state.set_resolution(cs.width, cs.height);
                    cs.force_full_update = true;

                    // Send DesktopSize as a single write (header + pseudo-rect).
                    let mut resize_msg = rfb::FramebufferUpdate {
                        message_type: rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE,
                        padding: 0,
                        rectangle_count: 1.into(),
                    }
                    .as_bytes()
                    .to_vec();
                    resize_msg.extend_from_slice(
                        rfb::Rectangle {
                            x: 0.into(),
                            y: 0.into(),
                            width: cs.width.into(),
                            height: cs.height.into(),
                            encoding_type: rfb::ENCODING_TYPE_DESKTOP_SIZE.into(),
                        }
                        .as_bytes(),
                    );
                    self.socket.write_all(&resize_msg).await?;
                }

                // Collect dirty rectangles.
                let dirty = update_state.collect_dirty(
                    &mut self.fb,
                    &mut self.dirty_recv,
                    cs.force_full_update,
                    &self.missed_dirty,
                );

                if !dirty.rects.is_empty() || cs.send_cursor {
                    tracing::trace!(
                        dirty_rects = dirty.rects.len(),
                        source = dirty.source.as_str(),
                        encoding = if cs.supports_zlib { "zlib" } else { "raw" },
                        "sending update"
                    );
                    if dirty.source == DirtySource::Full && !update_state.prev_fb.is_empty() {
                        tracelimit::warn_ratelimited!("full-screen retransmit triggered");
                    }

                    // Any FramebufferUpdate (data rects or cursor-only)
                    // consumes the pending update request.
                    cs.ready_for_update = false;
                    cs.force_full_update = false;

                    // Build the entire FramebufferUpdate into a single buffer
                    // to reduce async write calls from O(rects * 3) to O(1).
                    encoder.output_buf.clear();

                    let extra_rects = if cs.send_cursor { 1u16 } else { 0 };
                    // Cap rectangle count to u16::MAX to prevent overflow.
                    // At 8K+ resolutions with worst-case checkerboard patterns
                    // the tile count could exceed 65535.
                    let max_data_rects = (u16::MAX - extra_rects) as usize;
                    let send_rects = dirty.rects.len().min(max_data_rects);
                    encoder.output_buf.extend_from_slice(
                        rfb::FramebufferUpdate {
                            message_type: rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE,
                            padding: 0,
                            rectangle_count: (send_rects as u16 + extra_rects).into(),
                        }
                        .as_bytes(),
                    );
                    if dirty.rects.len() > max_data_rects {
                        // Some rects will be dropped. Force a full refresh
                        // on the next cycle to cover the missing regions.
                        cs.force_full_update = true;
                        tracing::warn!(
                            total = dirty.rects.len(),
                            sent = send_rects,
                            "dirty rect count exceeds u16::MAX, forcing full refresh next cycle"
                        );
                    }

                    if cs.send_cursor {
                        cs.send_cursor = false;
                        let (pixels, mask) = build_cursor(&cs.pixel_conv);
                        encoder.output_buf.extend_from_slice(
                            rfb::Rectangle {
                                x: 0.into(),
                                y: 0.into(),
                                width: 18.into(),
                                height: 18.into(),
                                encoding_type: rfb::ENCODING_TYPE_CURSOR.into(),
                            }
                            .as_bytes(),
                        );
                        encoder.output_buf.extend_from_slice(&pixels);
                        encoder.output_buf.extend_from_slice(&mask);
                    }

                    let use_zlib = cs.supports_zlib;
                    let mut bytes_sent: usize = 0;

                    for r in dirty.rects.iter().take(send_rects) {
                        debug_assert!(
                            r.x + r.w <= cs.width && r.y + r.h <= cs.height,
                            "dirty rect ({},{} {}x{}) exceeds framebuffer ({}x{})",
                            r.x,
                            r.y,
                            r.w,
                            r.h,
                            cs.width,
                            cs.height
                        );
                        bytes_sent += encoder.encode_rect(
                            &update_state.cur_fb,
                            cs.width,
                            &cs.pixel_conv,
                            r,
                            use_zlib,
                        )?;
                    }

                    // Single socket write for the entire update message.
                    self.socket.write_all(&encoder.output_buf).await?;

                    tracing::debug!(
                        dirty_rects = dirty.rects.len(),
                        bytes_sent,
                        source = dirty.source.as_str(),
                        "update sent"
                    );
                    // Return Vec for reuse next cycle.
                    update_state.recycle_rects(dirty.rects);
                }

                update_state.commit();
            }

            if socket_ready {
                self.handle_client_message(message_type, &mut cs).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::local::block_with_io;
    use pal_async::socket::PolledSocket;
    use pal_async::timer::PolledTimer;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use parking_lot::Mutex;
    use std::thread;
    use std::time::Duration;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;

    // -- Mock framebuffer: fixed-size pixel buffer with controllable content --

    struct MockFramebuffer {
        pixels: Vec<u32>,
        width: u16,
        height: u16,
    }

    impl MockFramebuffer {
        fn new(width: u16, height: u16, fill: u32) -> Self {
            Self {
                pixels: vec![fill; width as usize * height as usize],
                width,
                height,
            }
        }

        /// Set a single pixel.
        fn set(&mut self, x: u16, y: u16, color: u32) {
            self.pixels[y as usize * self.width as usize + x as usize] = color;
        }
    }

    impl Framebuffer for MockFramebuffer {
        fn resolution(&mut self) -> (u16, u16) {
            (self.width, self.height)
        }

        fn read_line(&mut self, line: u16, data: &mut [u8]) {
            let start = line as usize * self.width as usize;
            let end = start + self.width as usize;
            data.copy_from_slice(self.pixels[start..end].as_bytes());
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum InputEvent {
        Key(u16, bool),
        Mouse(u8, u16, u16),
    }

    #[derive(Clone)]
    struct RecordingInput {
        events: Arc<Mutex<Vec<InputEvent>>>,
    }

    impl RecordingInput {
        fn new(events: Arc<Mutex<Vec<InputEvent>>>) -> Self {
            Self { events }
        }
    }

    impl Input for RecordingInput {
        fn key(&mut self, scancode: u16, is_down: bool) {
            self.events
                .lock()
                .push(InputEvent::Key(scancode, is_down));
        }

        fn mouse(&mut self, button_mask: u8, x: u16, y: u16) {
            self.events
                .lock()
                .push(InputEvent::Mouse(button_mask, x, y));
        }
    }

    #[derive(Clone)]
    struct SharedFramebuffer(Arc<Mutex<MockFramebuffer>>);

    impl SharedFramebuffer {
        fn new(width: u16, height: u16, fill: u32) -> Self {
            Self(Arc::new(Mutex::new(MockFramebuffer::new(
                width, height, fill,
            ))))
        }

        fn set(&self, x: u16, y: u16, color: u32) {
            self.0.lock().set(x, y, color);
        }

        fn resize(&self, width: u16, height: u16, fill: u32) {
            *self.0.lock() = MockFramebuffer::new(width, height, fill);
        }
    }

    impl Framebuffer for SharedFramebuffer {
        fn resolution(&mut self) -> (u16, u16) {
            self.0.lock().resolution()
        }

        fn read_line(&mut self, line: u16, data: &mut [u8]) {
            self.0.lock().read_line(line, data)
        }
    }

    fn pixel(r: u8, g: u8, b: u8) -> u32 {
        (r as u32) << 16 | (g as u32) << 8 | b as u32
    }

    fn read_message<T: FromZeros + FromBytes + IntoBytes>(stream: &mut TcpStream) -> T {
        let mut message = T::new_zeroed();
        stream.read_exact(message.as_mut_bytes()).unwrap();
        message
    }

    fn write_message<T: IntoBytes + Immutable>(stream: &mut TcpStream, message: &T) {
        stream.write_all(message.as_bytes()).unwrap();
    }

    fn handshake(stream: &mut TcpStream, version: [u8; 12]) -> rfb::ServerInit {
        let server_version: rfb::ProtocolVersion = read_message(stream);
        assert_eq!(server_version.0, rfb::PROTOCOL_VERSION_38);

        stream.write_all(&version).unwrap();
        match version {
            rfb::PROTOCOL_VERSION_33 => {
                let security: rfb::Security33 = read_message(stream);
                assert_eq!(security.security_type, rfb::SECURITY_TYPE_NONE);
            }
            rfb::PROTOCOL_VERSION_37 | rfb::PROTOCOL_VERSION_38 => {
                let security: rfb::Security37 = read_message(stream);
                assert_eq!(security.type_count, 1);
                let mut types = [0u8; 1];
                stream.read_exact(&mut types).unwrap();
                assert_eq!(types, [rfb::SECURITY_TYPE_NONE]);
                stream.write_all(&[rfb::SECURITY_TYPE_NONE]).unwrap();

                if version == rfb::PROTOCOL_VERSION_38 {
                    let result: rfb::SecurityResult = read_message(stream);
                    assert_eq!(result.status.get(), rfb::SECURITY_RESULT_STATUS_OK);
                }
            }
            other => panic!("unexpected test version {other:?}"),
        }

        write_message(stream, &rfb::ClientInit { shared_flag: 1 });
        let init: rfb::ServerInit = read_message(stream);
        let mut name = vec![0; init.name_length.get() as usize];
        stream.read_exact(&mut name).unwrap();
        assert_eq!(name, b"test framebuffer");
        init
    }

    fn send_framebuffer_update_request(
        stream: &mut TcpStream,
        incremental: bool,
        width: u16,
        height: u16,
    ) {
        write_message(
            stream,
            &rfb::FramebufferUpdateRequest {
                message_type: rfb::CS_MESSAGE_FRAMEBUFFER_UPDATE_REQUEST,
                incremental: incremental.into(),
                x: 0.into(),
                y: 0.into(),
                width: width.into(),
                height: height.into(),
            },
        );
    }

    fn send_pointer_event(stream: &mut TcpStream, button_mask: u8, x: u16, y: u16) {
        write_message(
            stream,
            &rfb::PointerEvent {
                message_type: rfb::CS_MESSAGE_POINTER_EVENT,
                button_mask,
                x: x.into(),
                y: y.into(),
            },
        );
    }

    fn send_key_event(stream: &mut TcpStream, down: bool, key: u32) {
        write_message(
            stream,
            &rfb::KeyEvent {
                message_type: rfb::CS_MESSAGE_KEY_EVENT,
                down_flag: down.into(),
                padding: [0; 2],
                key: key.into(),
            },
        );
    }

    fn send_qemu_key_event(stream: &mut TcpStream, down: bool, scancode: u16) {
        write_message(
            stream,
            &rfb::QemuExtendedKeyEvent {
                message_type: rfb::CS_MESSAGE_QEMU,
                submessage_type: rfb::QEMU_MESSAGE_EXTENDED_KEY_EVENT,
                down_flag: (down as u16).into(),
                keysym: 0u32.into(),
                keycode: (scancode as u32).into(),
            },
        );
    }

    fn send_client_cut_text(stream: &mut TcpStream, text: &[u8]) {
        write_message(
            stream,
            &rfb::ClientCutText {
                message_type: rfb::CS_MESSAGE_CLIENT_CUT_TEXT,
                padding: [0; 3],
                length: (text.len() as u32).into(),
            },
        );
        stream.write_all(text).unwrap();
    }

    fn send_set_encodings(stream: &mut TcpStream, encodings: &[u32]) {
        write_message(
            stream,
            &rfb::SetEncodings {
                message_type: rfb::CS_MESSAGE_SET_ENCODINGS,
                padding: 0,
                encoding_count: (encodings.len() as u16).into(),
            },
        );
        for &encoding in encodings {
            let encoding: zerocopy::U32<zerocopy::BE> = encoding.into();
            stream.write_all(encoding.as_bytes()).unwrap();
        }
    }

    #[derive(Debug)]
    struct ReceivedRect {
        header: rfb::Rectangle,
        payload: Vec<u8>,
    }

    fn read_framebuffer_update(stream: &mut TcpStream) -> Vec<ReceivedRect> {
        let update: rfb::FramebufferUpdate = read_message(stream);
        assert_eq!(update.message_type, rfb::SC_MESSAGE_TYPE_FRAMEBUFFER_UPDATE);

        let mut rects = Vec::new();
        for _ in 0..update.rectangle_count.get() {
            let header: rfb::Rectangle = read_message(stream);
            let payload = match header.encoding_type.get() {
                rfb::ENCODING_TYPE_RAW => {
                    let payload_len =
                        header.width.get() as usize * header.height.get() as usize * 4;
                    let mut payload = vec![0; payload_len];
                    stream.read_exact(&mut payload).unwrap();
                    payload
                }
                rfb::ENCODING_TYPE_ZLIB => {
                    let mut len = [0; 4];
                    stream.read_exact(&mut len).unwrap();
                    let compressed_len = u32::from_be_bytes(len) as usize;
                    let mut payload = vec![0; 4 + compressed_len];
                    payload[..4].copy_from_slice(&len);
                    stream.read_exact(&mut payload[4..]).unwrap();
                    payload
                }
                rfb::ENCODING_TYPE_CURSOR => {
                    let pixels_len = header.width.get() as usize * header.height.get() as usize * 4;
                    let mask_bytes_per_row = (header.width.get() as usize).div_ceil(8);
                    let mask_len = mask_bytes_per_row * header.height.get() as usize;
                    let mut payload = vec![0; pixels_len + mask_len];
                    stream.read_exact(&mut payload).unwrap();
                    payload
                }
                rfb::ENCODING_TYPE_DESKTOP_SIZE | rfb::ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT => {
                    Vec::new()
                }
                other => panic!("unsupported test encoding {other:#x}"),
            };
            rects.push(ReceivedRect { header, payload });
        }
        rects
    }

    fn run_server_test<F>(
        fb: SharedFramebuffer,
        dirty_recv: Option<DirtyRectReceiver>,
        missed_dirty: Option<Arc<AtomicBool>>,
        client: F,
    ) -> (Result<(), Error>, Vec<InputEvent>)
    where
        F: FnOnce(TcpStream, SharedFramebuffer) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let input = RecordingInput::new(events.clone());
        let client_fb = fb.clone();

        let client = thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client(stream, client_fb);
        });

        let result = block_with_io(async |driver| -> Result<(), Error> {
            let mut listener = PolledSocket::new(&driver, listener).unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let socket = PolledSocket::new(&driver, socket.into()).unwrap();

            let mut server = Server::new(
                "test framebuffer".into(),
                socket,
                fb,
                input,
                dirty_recv,
                missed_dirty,
            );
            let mut updater = server.updater();
            let mut timer = PolledTimer::new(&driver);
            let update_task = async move {
                loop {
                    timer.sleep(Duration::from_millis(5)).await;
                    updater.update();
                }
            };

            futures::select! {
                result = server.run().fuse() => result,
                _ = update_task.fuse() => unreachable!(),
            }
        });

        client.join().unwrap();
        let events = events.lock().clone();
        (result, events)
    }

    // -- convert_pixels tests --

    #[test]
    fn e2e_handshake_supports_rfb_33_37_and_38() {
        for version in [
            rfb::PROTOCOL_VERSION_33,
            rfb::PROTOCOL_VERSION_37,
            rfb::PROTOCOL_VERSION_38,
        ] {
            let fb = SharedFramebuffer::new(2, 1, pixel(0x11, 0x22, 0x33));
            let (result, events) = run_server_test(fb, None, None, move |mut stream, _| {
                let init = handshake(&mut stream, version);
                assert_eq!(init.framebuffer_width.get(), 2);
                assert_eq!(init.framebuffer_height.get(), 1);
                assert_eq!(init.server_pixel_format.bits_per_pixel, 32);
            });

            assert!(matches!(result, Ok(())), "version {version:?}: {result:?}");
            assert!(events.is_empty());
        }
    }

    #[test]
    fn e2e_first_update_request_returns_full_raw_framebuffer() {
        let fb = SharedFramebuffer::new(2, 1, 0);
        fb.set(0, 0, pixel(0x12, 0x34, 0x56));
        fb.set(1, 0, pixel(0xab, 0xcd, 0xef));

        let (result, events) = run_server_test(fb, None, None, |mut stream, _| {
            let init = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            send_framebuffer_update_request(
                &mut stream,
                false,
                init.framebuffer_width.get(),
                init.framebuffer_height.get(),
            );

            let rects = read_framebuffer_update(&mut stream);
            assert_eq!(rects.len(), 1);
            let rect = &rects[0];
            assert_eq!(rect.header.x.get(), 0);
            assert_eq!(rect.header.y.get(), 0);
            assert_eq!(rect.header.width.get(), 2);
            assert_eq!(rect.header.height.get(), 1);
            assert_eq!(rect.header.encoding_type.get(), rfb::ENCODING_TYPE_RAW);
            assert_eq!(
                rect.payload,
                [pixel(0x12, 0x34, 0x56), pixel(0xab, 0xcd, 0xef)].as_bytes()
            );
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert!(events.is_empty());
    }

    #[test]
    fn e2e_set_encodings_drains_oversized_list_and_keeps_stream_in_sync() {
        let fb = SharedFramebuffer::new(64, 32, 0);
        let (result, events) = run_server_test(fb, None, None, |mut stream, _| {
            let init = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            assert_eq!(init.framebuffer_width.get(), 64);
            assert_eq!(init.framebuffer_height.get(), 32);

            let encodings = vec![rfb::ENCODING_TYPE_RAW; 4097];
            send_set_encodings(&mut stream, &encodings);
            send_pointer_event(&mut stream, 3, u16::MAX, u16::MAX);
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert_eq!(events, vec![InputEvent::Mouse(3, 0x7fff, 0x7fff)]);
    }

    #[test]
    fn e2e_keysym_paste_releases_both_modifier_sides() {
        let fb = SharedFramebuffer::new(4, 4, 0);
        let (result, events) = run_server_test(fb, None, None, |mut stream, _| {
            let _ = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            send_client_cut_text(&mut stream, &[b'A', 246]);
            send_key_event(&mut stream, true, 0xffe4u32);
            send_key_event(&mut stream, true, 0xffeau32);
            send_key_event(&mut stream, true, b'P'.into());
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert_eq!(
            events,
            vec![
                InputEvent::Key(0xe01d, true),
                InputEvent::Key(0xe038, true),
                InputEvent::Key(0x1d, false),
                InputEvent::Key(0xe01d, false),
                InputEvent::Key(0x38, false),
                InputEvent::Key(0xe038, false),
                InputEvent::Key(0x2a, true),
                InputEvent::Key(0x1e, true),
                InputEvent::Key(0x2a, false),
                InputEvent::Key(0x1e, false),
                InputEvent::Key(0x38, true),
                InputEvent::Key(0x52, true),
                InputEvent::Key(0x52, false),
                InputEvent::Key(0x50, true),
                InputEvent::Key(0x50, false),
                InputEvent::Key(0x4b, true),
                InputEvent::Key(0x4b, false),
                InputEvent::Key(0x4d, true),
                InputEvent::Key(0x4d, false),
                InputEvent::Key(0x38, false),
            ]
        );
    }

    #[test]
    fn e2e_qemu_paste_releases_both_modifier_sides() {
        let fb = SharedFramebuffer::new(4, 4, 0);
        let (result, events) = run_server_test(fb, None, None, |mut stream, _| {
            let _ = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            send_client_cut_text(&mut stream, b"A");
            send_qemu_key_event(&mut stream, true, 0xe01d);
            send_qemu_key_event(&mut stream, true, 0xe038);
            send_qemu_key_event(&mut stream, true, 0x19);
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert_eq!(
            events,
            vec![
                InputEvent::Key(0xe01d, true),
                InputEvent::Key(0xe038, true),
                InputEvent::Key(0x1d, false),
                InputEvent::Key(0xe01d, false),
                InputEvent::Key(0x38, false),
                InputEvent::Key(0xe038, false),
                InputEvent::Key(0x2a, true),
                InputEvent::Key(0x1e, true),
                InputEvent::Key(0x2a, false),
                InputEvent::Key(0x1e, false),
            ]
        );
    }

    #[test]
    fn e2e_dirty_channel_close_falls_back_to_tile_diff() {
        #[expect(clippy::disallowed_methods)]
        let (tx, rx) = mpsc::channel(4);
        let fb = SharedFramebuffer::new(32, 32, 0);
        let tx = Arc::new(Mutex::new(Some(tx)));

        let (result, events) = run_server_test(fb, Some(rx), None, {
            let tx = tx.clone();
            move |mut stream, fb| {
                let init = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
                let width = init.framebuffer_width.get();
                let height = init.framebuffer_height.get();

                send_framebuffer_update_request(&mut stream, false, width, height);
                let initial = read_framebuffer_update(&mut stream);
                assert_eq!(initial.len(), 1);

                fb.set(2, 2, pixel(0xaa, 0xbb, 0xcc));
                tx.lock()
                    .as_mut()
                    .unwrap()
                    .try_send(Arc::new(vec![video_core::DirtyRect {
                        left: 0,
                        top: 0,
                        right: 16,
                        bottom: 16,
                    }]))
                    .unwrap();
                send_framebuffer_update_request(&mut stream, true, width, height);
                let device_dirty = read_framebuffer_update(&mut stream);
                assert_eq!(device_dirty.len(), 1);
                assert_eq!(
                    device_dirty[0].header.encoding_type.get(),
                    rfb::ENCODING_TYPE_RAW
                );

                tx.lock().take();
                fb.set(20, 20, pixel(0x11, 0x22, 0x33));
                send_framebuffer_update_request(&mut stream, true, width, height);
                let fallback = read_framebuffer_update(&mut stream);
                assert_eq!(fallback.len(), 1);
                assert_eq!(
                    fallback[0].header.encoding_type.get(),
                    rfb::ENCODING_TYPE_RAW
                );
                assert!(fallback[0].header.x.get() <= 20);
                assert!(fallback[0].header.x.get() + fallback[0].header.width.get() > 20);
                assert!(fallback[0].header.y.get() <= 20);
                assert!(fallback[0].header.y.get() + fallback[0].header.height.get() > 20);
            }
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert!(events.is_empty());
    }

    #[test]
    fn e2e_desktop_resize_sends_pseudo_rect_before_full_refresh() {
        let fb = SharedFramebuffer::new(4, 2, pixel(0x10, 0x20, 0x30));
        let (result, events) = run_server_test(fb, None, None, |mut stream, fb| {
            let init = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            send_set_encodings(&mut stream, &[rfb::ENCODING_TYPE_DESKTOP_SIZE]);
            send_framebuffer_update_request(
                &mut stream,
                false,
                init.framebuffer_width.get(),
                init.framebuffer_height.get(),
            );
            let initial = read_framebuffer_update(&mut stream);
            assert_eq!(initial.len(), 1);

            fb.resize(6, 3, pixel(0xaa, 0xbb, 0xcc));
            send_framebuffer_update_request(&mut stream, true, 6, 3);

            let resize = read_framebuffer_update(&mut stream);
            assert_eq!(resize.len(), 1);
            assert_eq!(
                resize[0].header.encoding_type.get(),
                rfb::ENCODING_TYPE_DESKTOP_SIZE
            );
            assert_eq!(resize[0].header.width.get(), 6);
            assert_eq!(resize[0].header.height.get(), 3);

            let refresh = read_framebuffer_update(&mut stream);
            assert_eq!(refresh.len(), 1);
            assert_eq!(refresh[0].header.width.get(), 6);
            assert_eq!(refresh[0].header.height.get(), 3);
            assert_eq!(refresh[0].payload.len(), 6 * 3 * 4);
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert!(events.is_empty());
    }

    #[test]
    fn e2e_cursor_and_zlib_updates_are_parsed_by_test_helper() {
        let fb = SharedFramebuffer::new(4, 2, pixel(0x10, 0x20, 0x30));
        let (result, events) = run_server_test(fb, None, None, |mut stream, _| {
            let init = handshake(&mut stream, rfb::PROTOCOL_VERSION_38);
            send_set_encodings(
                &mut stream,
                &[rfb::ENCODING_TYPE_CURSOR, rfb::ENCODING_TYPE_ZLIB],
            );
            send_framebuffer_update_request(
                &mut stream,
                false,
                init.framebuffer_width.get(),
                init.framebuffer_height.get(),
            );

            let update = read_framebuffer_update(&mut stream);
            assert_eq!(update.len(), 2);
            assert_eq!(
                update[0].header.encoding_type.get(),
                rfb::ENCODING_TYPE_CURSOR
            );
            assert_eq!(update[0].header.width.get(), 18);
            assert_eq!(update[0].header.height.get(), 18);
            assert!(!update[0].payload.is_empty());
            assert_eq!(
                update[1].header.encoding_type.get(),
                rfb::ENCODING_TYPE_ZLIB
            );
            assert!(!update[1].payload.is_empty());
        });

        assert!(matches!(result, Ok(())), "{result:?}");
        assert!(events.is_empty());
    }

    fn default_pc() -> PixelConversion {
        PixelConversion::from_format(&default_pixel_format())
    }

    fn default_pixel_format() -> rfb::PixelFormat {
        rfb::PixelFormat {
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
        }
    }

    #[test]
    fn convert_pixels_identity_32bpp() {
        // Default format matches internal layout -- should be a direct copy.
        let pc = default_pc();
        let src = [0x00FF0000u32, 0x0000FF00, 0x000000FF];
        let mut out = Vec::new();
        convert_pixels(&src, &pc, &mut out);
        assert_eq!(out, src.as_bytes());
    }

    #[test]
    fn convert_pixels_16bpp_rgb565() {
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 31.into(),   // 5 bits
            green_max: 63.into(), // 6 bits
            blue_max: 31.into(),  // 5 bits
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
            padding: [0; 3],
        });
        // Pure red: 0x00FF0000 -> R=31, G=0, B=0 -> (31 << 11) = 0xF800
        let src = [0x00FF0000u32];
        let mut out = Vec::new();
        convert_pixels(&src, &pc, &mut out);
        assert_eq!(out, 0xF800u16.to_le_bytes());
    }

    // -- UpdateState tile-diff tests --

    #[test]
    fn update_state_first_frame_is_full() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First call with force_full=true: every tile dirty.
        let result = state.collect_dirty(&mut fb, &mut None, true, &None);
        assert_eq!(result.source, DirtySource::Full);
        // 32/16 = 2 tiles per axis = 4 tiles, merged into 1 rect.
        assert!(!result.rects.is_empty());
        let total_pixels: u32 = result.rects.iter().map(|r| r.w as u32 * r.h as u32).sum();
        assert_eq!(total_pixels, 32 * 32);
        state.commit();
    }

    #[test]
    fn update_state_no_change_produces_no_rects() {
        let mut fb = MockFramebuffer::new(32, 32, 0xAABBCCDD);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame: full.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Second frame: nothing changed, should produce no dirty rects.
        let result = state.collect_dirty(&mut fb, &mut None, false, &None);
        assert_eq!(result.source, DirtySource::Diff);
        assert!(result.rects.is_empty());
        state.commit();
    }

    #[test]
    fn update_state_detects_single_pixel_change() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Change one pixel in tile (1,1).
        fb.set(20, 20, 0xFFFFFFFF);

        let result = state.collect_dirty(&mut fb, &mut None, false, &None);
        assert_eq!(result.source, DirtySource::Diff);
        assert_eq!(result.rects.len(), 1);
        // The dirty rect should cover the tile containing pixel (20,20).
        let r = &result.rects[0];
        assert!(r.x <= 20 && r.x + r.w > 20);
        assert!(r.y <= 20 && r.y + r.h > 20);
        state.commit();
    }

    #[test]
    fn update_state_device_dirty_uses_partial_read() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Simulate device dirty rect via mpsc channel.
        #[expect(clippy::disallowed_methods)]
        let (mut tx, rx) = mpsc::channel(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));

        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        // Change the pixel so there's actually something different in VRAM.
        fb.set(5, 5, 0x12345678);

        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);
        assert!(!result.rects.is_empty());
        state.commit();
    }

    #[test]
    fn update_state_prev_fb_valid_after_device_dirty() {
        // Verify that after a device-dirty cycle, prev_fb is complete
        // (non-dirty regions preserved) so a subsequent tile-diff works.
        let mut fb = MockFramebuffer::new(32, 32, 0xAAAAAAAA);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame: full.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Device-dirty cycle: only tile (0,0) reported dirty.
        #[expect(clippy::disallowed_methods)]
        let (mut tx, rx) = mpsc::channel(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        let _ = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        state.commit();

        // Third cycle: device dirty was seen, so empty channel means
        // "nothing changed" — skips the 8MB VRAM read entirely.
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);
        assert!(
            result.rects.is_empty(),
            "idle cycle should produce no dirty rects"
        );
        state.commit();
    }

    #[test]
    fn convert_pixels_empty_input() {
        let pc = default_pc();
        let mut out = Vec::new();
        convert_pixels(&[], &pc, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn convert_pixels_blue_channel_correct() {
        // Regression: shift_b previously used red_max instead of blue_max.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 31.into(),
            green_max: 63.into(),
            blue_max: 31.into(),
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
            padding: [0; 3],
        });
        // Pure blue: 0x000000FF -> R=0, G=0, B=31 -> (31 << 0) = 0x001F
        let src = [0x000000FFu32];
        let mut out = Vec::new();
        convert_pixels(&src, &pc, &mut out);
        assert_eq!(out, 0x001Fu16.to_le_bytes());
    }

    #[test]
    fn convert_pixels_rgb332_asymmetric() {
        // RGB332: red=3 bits (max=7), green=3 bits (max=7), blue=2 bits (max=3).
        // Different bit widths per channel — catches the old red_max-for-blue bug
        // and the count_ones vs leading_zeros bug simultaneously.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 8,
            depth: 8,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 7.into(),   // 3 bits
            green_max: 7.into(), // 3 bits
            blue_max: 3.into(),  // 2 bits
            red_shift: 5,
            green_shift: 2,
            blue_shift: 0,
            padding: [0; 3],
        });
        // Pure white: 0x00FFFFFF -> R=7, G=7, B=3 -> (7<<5)|(7<<2)|(3<<0) = 0xFF
        let src = [0x00FFFFFFu32];
        let mut out = Vec::new();
        convert_pixels(&src, &pc, &mut out);
        assert_eq!(out, [0xFFu8]);

        // Pure blue: 0x000000FF -> R=0, G=0, B=3 -> 3
        out.clear();
        convert_pixels(&[0x000000FFu32], &pc, &mut out);
        assert_eq!(out, [3u8]);

        // Pure red: 0x00FF0000 -> R=7, G=0, B=0 -> (7<<5) = 0xE0
        out.clear();
        convert_pixels(&[0x00FF0000u32], &pc, &mut out);
        assert_eq!(out, [0xE0u8]);
    }

    #[test]
    fn convert_pixels_zero_max_handled() {
        // A buggy client sends blue_max=0. Should not panic.
        // With our guard (default to 8 bits), shift_b = 0, so blue passes through.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 255.into(),
            green_max: 255.into(),
            blue_max: 0.into(), // buggy client
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            padding: [0; 3],
        });
        // Should not panic or produce garbage
        let src = [0x00112233u32];
        let mut out = Vec::new();
        convert_pixels(&src, &pc, &mut out);
        assert_eq!(out.len(), 4); // 32bpp output
    }
}
