// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-connection encoder: zlib state, scratch buffers, and rectangle
//! encoding (raw, zlib, or ZRLE). Also builds the software cursor shape.

use crate::Error;
use crate::Rect;
use crate::pixel::PixelConversion;
use crate::pixel::convert_pixels;
use crate::rfb;
use flate2::Compression;
use flate2::FlushCompress;
use zerocopy::IntoBytes;

/// Which wire encoding to use for a rectangle's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireEncoding {
    Raw,
    Zlib,
    Zrle,
}

/// Manages per-connection zlib state and scratch buffers for encoding
/// framebuffer rectangles.
pub(crate) struct Encoder {
    pub(crate) tile_buf: Vec<u8>,
    pub(crate) zlib_buf: Vec<u8>,
    /// Accumulates the entire FramebufferUpdate message before sending.
    pub(crate) output_buf: Vec<u8>,
    /// The Zlib encoding's continuous zlib stream. RFB requires a single
    /// continuous stream per connection. Created lazily on first use.
    pub(crate) zlib_stream: Option<flate2::Compress>,
    /// ZRLE's continuous zlib stream, separate from the Zlib one. Created
    /// lazily on first use.
    pub(crate) zrle_stream: Option<flate2::Compress>,
    /// Scratch buffer for ZRLE tile bytes before they are compressed.
    pub(crate) zrle_tile_buf: Vec<u8>,
    /// Scratch buffer holding one tile's pixels while its subencoding is chosen.
    pub(crate) zrle_pixels: Vec<u32>,
    /// Each pixel's palette index, filled during the palette build.
    pub(crate) zrle_indices: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self {
            tile_buf: Vec::new(),
            zlib_buf: Vec::new(),
            output_buf: Vec::new(),
            zlib_stream: None,
            zrle_stream: None,
            zrle_tile_buf: Vec::new(),
            zrle_pixels: Vec::new(),
            zrle_indices: Vec::new(),
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
        encoding: WireEncoding,
    ) -> Result<usize, Error> {
        // ZRLE reads cur_fb directly and emits per-tile CPIXELs, not the flat
        // tile_buf conversion below.
        if encoding == WireEncoding::Zrle {
            return self.append_zrle(cur_fb, fb_width, pc, rect);
        }

        self.tile_buf.clear();
        self.tile_buf
            .reserve(rect.w as usize * rect.h as usize * pc.dest_depth);

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

        match encoding {
            WireEncoding::Zlib => self.append_zlib(rect),
            WireEncoding::Raw => self.append_raw(rect),
            WireEncoding::Zrle => unreachable!("ZRLE handled above"),
        }
    }

    /// Compress `src` through one continuous RFB zlib `stream` (Sync flush)
    /// into `out`, growing `out` if incompressible data overruns it. Returns
    /// the compressed length.
    fn zlib_compress(
        stream: &mut flate2::Compress,
        src: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<usize, Error> {
        out.clear();
        out.resize(src.len() + 128, 0);
        let before_in = stream.total_in();
        let before_out = stream.total_out();
        loop {
            let in_offset = (stream.total_in() - before_in) as usize;
            let out_offset = (stream.total_out() - before_out) as usize;
            let status = stream
                .compress(
                    &src[in_offset..],
                    &mut out[out_offset..],
                    FlushCompress::Sync,
                )
                .map_err(Error::ZlibCompression)?;
            let out_used = (stream.total_out() - before_out) as usize;
            let in_done = (stream.total_in() - before_in) as usize >= src.len();
            // Done only when all input is consumed AND the call left output room:
            // a full buffer can mean the Sync-flush trailer is still pending, so
            // grow and call again rather than emit a stream missing its flush
            // boundary.
            if in_done && status == flate2::Status::Ok && out_used < out.len() {
                break;
            }
            // Buffer full (incompressible data or a pending flush trailer).
            if out_used >= out.len() - 16 {
                out.resize(out.len() * 2, 0);
            }
        }
        let compressed_len = (stream.total_out() - before_out) as usize;
        out.truncate(compressed_len);
        Ok(compressed_len)
    }

    /// Append a length-prefixed zlib blob plus its rectangle header to
    /// output_buf. Shared by the Zlib and ZRLE encodings.
    fn append_zlib_rect(&mut self, rect: &Rect, encoding: rfb::EncodingType) -> usize {
        self.output_buf.extend_from_slice(
            rfb::Rectangle {
                x: rect.x.into(),
                y: rect.y.into(),
                width: rect.w.into(),
                height: rect.h.into(),
                encoding_type: encoding.wire_u32().into(),
            }
            .as_bytes(),
        );
        self.output_buf
            .extend_from_slice(&(self.zlib_buf.len() as u32).to_be_bytes());
        self.output_buf.extend_from_slice(&self.zlib_buf);
        // rect header (12) + length prefix (4) + compressed data
        12 + 4 + self.zlib_buf.len()
    }

    /// Compress tile_buf with the Zlib-encoding stream and append to output_buf.
    fn append_zlib(&mut self, rect: &Rect) -> Result<usize, Error> {
        let stream = self
            .zlib_stream
            .get_or_insert_with(|| flate2::Compress::new(Compression::fast(), true));
        Self::zlib_compress(stream, &self.tile_buf, &mut self.zlib_buf)?;
        Ok(self.append_zlib_rect(rect, rfb::EncodingType::ZLIB))
    }

    /// Encode a rectangle with ZRLE: split it into 64x64 tiles, choose the
    /// smallest subencoding for each (solid, packed palette, palette RLE, plain
    /// RLE, or raw), then compress the tile stream through the ZRLE zlib stream.
    fn append_zrle(
        &mut self,
        cur_fb: &[u32],
        fb_width: u16,
        pc: &PixelConversion,
        rect: &Rect,
    ) -> Result<usize, Error> {
        debug_assert!(
            rect.w == 0
                || rect.h == 0
                || (rect.y as usize + rect.h as usize - 1) * fb_width as usize
                    + (rect.x as usize + rect.w as usize)
                    <= cur_fb.len(),
            "ZRLE rect exceeds the framebuffer"
        );
        self.zrle_tile_buf.clear();
        let tile = rfb::ZRLE_TILE;
        let mut ty = 0u16;
        while ty < rect.h {
            let th = (rect.h - ty).min(tile);
            let mut tx = 0u16;
            while tx < rect.w {
                let tw = (rect.w - tx).min(tile);
                let tile_x0 = (rect.x + tx) as usize;
                self.zrle_pixels.clear();
                for row in 0..th {
                    let base = (rect.y + ty + row) as usize * fb_width as usize + tile_x0;
                    self.zrle_pixels
                        .extend_from_slice(&cur_fb[base..base + tw as usize]);
                }
                encode_zrle_tile(
                    &self.zrle_pixels,
                    tw as usize,
                    pc,
                    &mut self.zrle_tile_buf,
                    &mut self.zrle_indices,
                );
                tx += tw;
            }
            ty += th;
        }
        let stream = self
            .zrle_stream
            .get_or_insert_with(|| flate2::Compress::new(Compression::fast(), true));
        Self::zlib_compress(stream, &self.zrle_tile_buf, &mut self.zlib_buf)?;
        Ok(self.append_zlib_rect(rect, rfb::EncodingType::ZRLE))
    }

    /// Append tile_buf as raw (uncompressed) rect to output_buf.
    fn append_raw(&mut self, rect: &Rect) -> Result<usize, Error> {
        self.output_buf.extend_from_slice(
            rfb::Rectangle {
                x: rect.x.into(),
                y: rect.y.into(),
                width: rect.w.into(),
                height: rect.h.into(),
                encoding_type: rfb::EncodingType::RAW.wire_u32().into(),
            }
            .as_bytes(),
        );
        self.output_buf.extend_from_slice(&self.tile_buf);
        // rect header (12) + raw pixel data
        Ok(12 + self.tile_buf.len())
    }
}

/// Bytes needed to encode a ZRLE run length value `k` (the run length minus
/// one): a 255 for each full 255, then a final remainder byte.
fn run_len_bytes(k: usize) -> usize {
    k / 255 + 1
}

/// Append a ZRLE run length value `k` (= run length - 1): 255 for each full
/// 255, then the remainder. The decoder sums the bytes and adds one.
fn write_run_len(mut k: usize, out: &mut Vec<u8>) {
    while k >= 255 {
        out.push(255);
        k -= 255;
    }
    out.push(k as u8);
}

/// Encode one tile's pixels (scanline order, `tw` pixels wide) into `out`,
/// choosing the smallest of the applicable ZRLE subencodings. The candidates:
/// solid (1 colour), packed palette (<=16 colours, indices packed MSB-first and
/// padded to a byte per row), palette RLE (<=127 colours, runs of palette
/// indices), plain RLE (runs of CPIXELs), and raw. `indices` receives each
/// pixel's palette index during the build pass.
fn encode_zrle_tile(
    pixels: &[u32],
    tw: usize,
    pc: &PixelConversion,
    out: &mut Vec<u8>,
    indices: &mut Vec<u8>,
) {
    debug_assert!(tw > 0, "ZRLE tile width must be non-zero");
    let cp = pc.cpixel_depth;
    let n = pixels.len();

    // Build the palette (first-seen order) and each pixel's index in one pass.
    // Abandoned (palette = None) once it would exceed 127 colours, the most a
    // palette subencoding can carry.
    indices.clear();
    let mut palette: Option<Vec<u32>> = Some(Vec::new());
    for &p in pixels {
        let pal = palette.as_mut().unwrap();
        match pal.iter().position(|&c| c == p) {
            Some(i) => indices.push(i as u8),
            None => {
                if pal.len() == 127 {
                    palette = None;
                    break;
                }
                pal.push(p);
                indices.push((pal.len() - 1) as u8);
            }
        }
    }

    // Solid tile: a single CPIXEL.
    if let Some(pal) = &palette {
        if pal.len() == 1 {
            out.push(1);
            pc.push_cpixel(pal[0], out);
            return;
        }
    }

    // Runs of identical pixels in scanline order (they may cross tile rows).
    let mut runs: Vec<(u32, usize)> = Vec::new();
    for &p in pixels {
        match runs.last_mut() {
            Some((c, len)) if *c == p => *len += 1,
            _ => runs.push((p, 1)),
        }
    }

    let bpp = |len: usize| {
        if len <= 2 {
            1
        } else if len <= 4 {
            2
        } else {
            4
        }
    };

    // Candidate sizes.
    let raw_size = n * cp;
    let plain_rle_size: usize = runs.iter().map(|(_, l)| cp + run_len_bytes(l - 1)).sum();
    let packed_size = palette
        .as_ref()
        .filter(|pal| pal.len() <= 16)
        .map(|pal| pal.len() * cp + (tw * bpp(pal.len())).div_ceil(8) * (n / tw));
    let palette_rle_size = palette.as_ref().map(|pal| {
        let runs_bytes: usize = runs
            .iter()
            .map(|(_, l)| if *l == 1 { 1 } else { 1 + run_len_bytes(l - 1) })
            .sum();
        pal.len() * cp + runs_bytes
    });

    // Smallest applicable candidate wins; raw is always available.
    enum Choice {
        Raw,
        Packed,
        PaletteRle,
        PlainRle,
    }
    let mut best = (raw_size, Choice::Raw);
    if let Some(s) = packed_size {
        if s < best.0 {
            best = (s, Choice::Packed);
        }
    }
    if let Some(s) = palette_rle_size {
        if s < best.0 {
            best = (s, Choice::PaletteRle);
        }
    }
    if plain_rle_size < best.0 {
        best = (plain_rle_size, Choice::PlainRle);
    }

    match best.1 {
        Choice::Raw => {
            out.push(0);
            for &p in pixels {
                pc.push_cpixel(p, out);
            }
        }
        Choice::Packed => {
            let pal = palette.as_ref().unwrap();
            out.push(pal.len() as u8); // subencoding 2..=16 = palette size
            for &c in pal {
                pc.push_cpixel(c, out);
            }
            let bpp = bpp(pal.len());
            let th = n / tw;
            for row in 0..th {
                let mut acc = 0u8;
                let mut nbits = 0u32;
                for col in 0..tw {
                    acc = (acc << bpp) | indices[row * tw + col];
                    nbits += bpp as u32;
                    if nbits == 8 {
                        out.push(acc);
                        acc = 0;
                        nbits = 0;
                    }
                }
                if nbits > 0 {
                    out.push(acc << (8 - nbits)); // pad the row's last byte
                }
            }
        }
        Choice::PaletteRle => {
            let pal = palette.as_ref().unwrap();
            out.push(128 + pal.len() as u8); // subencoding 130..=255
            for &c in pal {
                pc.push_cpixel(c, out);
            }
            // Each run's palette index is its first pixel's precomputed index.
            let mut off = 0usize;
            for &(_, len) in &runs {
                let idx = indices[off];
                if len == 1 {
                    out.push(idx);
                } else {
                    out.push(idx | 0x80);
                    write_run_len(len - 1, out);
                }
                off += len;
            }
        }
        Choice::PlainRle => {
            out.push(128);
            for &(c, len) in &runs {
                pc.push_cpixel(c, out);
                write_run_len(len - 1, out);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Decompress;
    use flate2::FlushDecompress;

    fn pc_32bpp() -> PixelConversion {
        PixelConversion::from_format(&rfb::PixelFormat {
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
        })
    }

    /// Parse the single ZRLE rectangle in `output_buf`: confirm the header
    /// encoding, then inflate the length-prefixed blob and return the tile
    /// bytes.
    fn inflate_zrle(output: &[u8]) -> Vec<u8> {
        // Rectangle header: x,y (4) + w,h (4) + encoding_type (4) = 12 bytes.
        let enc = u32::from_be_bytes(output[8..12].try_into().unwrap());
        assert_eq!(enc as i32, rfb::EncodingType::ZRLE.0);
        let len = u32::from_be_bytes(output[12..16].try_into().unwrap()) as usize;
        let blob = &output[16..16 + len];
        let mut d = Decompress::new(true);
        let mut out = vec![0u8; 64 * 1024];
        d.decompress(blob, &mut out, FlushDecompress::Sync).unwrap();
        out.truncate(d.total_out() as usize);
        out
    }

    #[test]
    fn zrle_solid_tile_is_one_cpixel() {
        let mut enc = Encoder::new();
        let pc = pc_32bpp();
        // 4x2 framebuffer, all one colour.
        let fb = vec![0x0011_2233u32; 8];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        };
        enc.encode_rect(&fb, 4, &pc, &rect, WireEncoding::Zrle)
            .unwrap();
        // One tile, solid: subencoding byte 1 + CPIXEL [B, G, R].
        assert_eq!(inflate_zrle(&enc.output_buf), [1, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn zrle_mixed_tile_is_raw_cpixels() {
        let mut enc = Encoder::new();
        let pc = pc_32bpp();
        // 2x1: red then blue, so the tile is not solid.
        let fb = vec![0x00FF_0000u32, 0x0000_00FFu32];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
        };
        enc.encode_rect(&fb, 2, &pc, &rect, WireEncoding::Zrle)
            .unwrap();
        // One tile, raw: subencoding byte 0 + CPIXEL(red) + CPIXEL(blue).
        assert_eq!(
            inflate_zrle(&enc.output_buf),
            [0, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]
        );
    }

    #[test]
    fn zrle_solid_subrect_indexes_with_stride() {
        // A 2x2 solid tile inside a wider 4-pixel-stride framebuffer: the tile
        // must read the right columns, not run off the stride.
        let mut enc = Encoder::new();
        let pc = pc_32bpp();
        // 4x2 buffer; the (x=1,y=0 2x2) region is solid 0x00445566, rest differs.
        let fb = vec![
            0x00000001,
            0x0044_5566,
            0x0044_5566,
            0x00000002, // row 0
            0x00000003,
            0x0044_5566,
            0x0044_5566,
            0x00000004, // row 1
        ];
        let rect = Rect {
            x: 1,
            y: 0,
            w: 2,
            h: 2,
        };
        enc.encode_rect(&fb, 4, &pc, &rect, WireEncoding::Zrle)
            .unwrap();
        assert_eq!(inflate_zrle(&enc.output_buf), [1, 0x66, 0x55, 0x44]);
    }

    fn first_subencoding(pixels: &[u32], w: u16, h: u16) -> u8 {
        let mut enc = Encoder::new();
        enc.encode_rect(
            pixels,
            w,
            &pc_32bpp(),
            &Rect { x: 0, y: 0, w, h },
            WireEncoding::Zrle,
        )
        .unwrap();
        inflate_zrle(&enc.output_buf)[0]
    }

    #[test]
    fn zrle_chooses_packed_palette() {
        // 4x4 two-colour checkerboard: no runs, few colours -> packed palette
        // (subencoding = palette size 2).
        let (a, b) = (0x0011_2233u32, 0x00aa_bbccu32);
        let px: Vec<u32> = (0..16)
            .map(|i| if ((i / 4) + (i % 4)) % 2 == 0 { a } else { b })
            .collect();
        assert_eq!(first_subencoding(&px, 4, 4), 2);
    }

    #[test]
    fn zrle_chooses_palette_rle() {
        // 64x64 of alternating solid rows: 2 colours in long runs -> palette
        // RLE (subencoding 128 + palette size 2 = 130).
        let (a, b) = (0x0010_2030u32, 0x0040_5060u32);
        let px: Vec<u32> = (0..64 * 64)
            .map(|i| if (i / 64) % 2 == 0 { a } else { b })
            .collect();
        assert_eq!(first_subencoding(&px, 64, 64), 130);
    }

    #[test]
    fn zrle_chooses_plain_rle() {
        // 64x1 with >16 colours in short runs: no packed palette, and the runs
        // make plain RLE smaller than a 22-entry palette -> plain RLE (128).
        let px: Vec<u32> = (0..64).map(|i| (i as u32 / 3) + 1).collect();
        assert_eq!(first_subencoding(&px, 64, 1), 128);
    }

    #[test]
    fn run_len_encoding_handles_255_continuation() {
        // write_run_len emits a 255 for each full 255 then the remainder; the
        // decoder sums every byte to recover the value. run_len_bytes predicts
        // the byte count used when sizing the candidates.
        let cases: &[(usize, &[u8])] = &[
            (0, &[0]),
            (254, &[254]),
            (255, &[255, 0]),
            (509, &[255, 254]),
            (510, &[255, 255, 0]),
            (765, &[255, 255, 255, 0]),
        ];
        for &(k, expected) in cases {
            assert_eq!(run_len_bytes(k), expected.len(), "byte count for {k}");
            let mut out = Vec::new();
            write_run_len(k, &mut out);
            assert_eq!(out, expected, "encoding for {k}");
            assert_eq!(
                out.iter().map(|&b| b as usize).sum::<usize>(),
                k,
                "sum for {k}"
            );
        }
    }

    #[test]
    fn zrle_packed_row_spans_multiple_bytes_with_padding() {
        // 6x1, 4 colours -> bpp 2, so a row is 12 index bits = 2 bytes with 4
        // padding bits. Pins the MSB-first packing and the zero-padded tail byte.
        let mut enc = Encoder::new();
        let c = [0x0001_0203u32, 0x0004_0506, 0x0007_0809, 0x000a_0b0c];
        let px = vec![c[0], c[1], c[2], c[3], c[0], c[1]];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 6,
            h: 1,
        };
        enc.encode_rect(&px, 6, &pc_32bpp(), &rect, WireEncoding::Zrle)
            .unwrap();
        // subencoding 4, then 4 CPIXELs [B,G,R], then indices 0,1,2,3,0,1 packed
        // MSB-first at 2 bits: 0b00_01_10_11 = 0x1B, 0b00_01_0000 = 0x10.
        assert_eq!(
            inflate_zrle(&enc.output_buf),
            [4, 3, 2, 1, 6, 5, 4, 9, 8, 7, 0x0c, 0x0b, 0x0a, 0x1B, 0x10]
        );
    }

    #[test]
    fn zrle_long_run_emits_255_continuation() {
        // 64x6 of one colour plus a final different pixel: plain RLE with a 383
        // run, so the run length (382) is written as [255, 127].
        let (a, b) = (0x0011_2233u32, 0x0044_5566u32);
        let mut px = vec![a; 64 * 6];
        px[64 * 6 - 1] = b;
        let mut enc = Encoder::new();
        let rect = Rect {
            x: 0,
            y: 0,
            w: 64,
            h: 6,
        };
        enc.encode_rect(&px, 64, &pc_32bpp(), &rect, WireEncoding::Zrle)
            .unwrap();
        // subencoding 128, CPIXEL(a), run-len 382 = [255,127], CPIXEL(b), run-len 0.
        assert_eq!(
            inflate_zrle(&enc.output_buf),
            [128, 0x33, 0x22, 0x11, 255, 127, 0x66, 0x55, 0x44, 0]
        );
    }
}
