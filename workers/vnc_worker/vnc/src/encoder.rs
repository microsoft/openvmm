// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-connection encoder: zlib state, scratch buffers, and rectangle
//! encoding (raw or zlib). Also builds the software cursor shape.

use crate::Error;
use crate::Rect;
use crate::pixel::PixelConversion;
use crate::pixel::convert_pixels;
use crate::rfb;
use flate2::Compression;
use flate2::FlushCompress;
use zerocopy::IntoBytes;

/// Manages per-connection zlib state and scratch buffers for encoding
/// framebuffer rectangles.
pub(crate) struct Encoder {
    pub(crate) tile_buf: Vec<u8>,
    pub(crate) zlib_buf: Vec<u8>,
    /// Accumulates the entire FramebufferUpdate message before sending,
    /// reducing multiple socket writes to a single write_all.
    pub(crate) output_buf: Vec<u8>,
    /// RFB requires a single continuous zlib stream per connection.
    pub(crate) zlib_stream: flate2::Compress,
}

impl Encoder {
    pub(crate) fn new() -> Self {
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
    pub(crate) fn encode_rect(
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
            let status = self
                .zlib_stream
                .compress(
                    &self.tile_buf[in_offset..],
                    &mut self.zlib_buf[out_offset..],
                    FlushCompress::Sync,
                )
                .map_err(Error::ZlibCompression)?;
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

/// Build the default 18x18 arrow cursor as a VNC cursor pseudo-encoding.
/// Returns (pixel_data, mask_data) in the client's pixel format.
pub(crate) fn build_cursor(pc: &PixelConversion) -> (Vec<u8>, Vec<u8>) {
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
