// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pixel format conversion: cached conversion parameters and the per-pixel
//! conversion routine that emits client-formatted bytes from internal
//! 0x00RRGGBB pixels.

use crate::rfb;
use zerocopy::IntoBytes;

/// Pre-computed pixel conversion parameters, cached per-connection.
#[derive(Clone, Copy)]
pub(crate) struct PixelConversion {
    pub(crate) dest_depth: usize,
    pub(crate) shift_r: u32,
    pub(crate) shift_g: u32,
    pub(crate) shift_b: u32,
    pub(crate) out_shift_r: u8,
    pub(crate) out_shift_g: u8,
    pub(crate) out_shift_b: u8,
    pub(crate) big_endian: bool,
    /// True when the client's format matches our internal 0x00RRGGBB layout
    /// and we can emit pixels as-is without per-pixel conversion.
    pub(crate) no_convert: bool,
    /// Bytes per ZRLE CPIXEL: 3 for a 32bpp true-colour format whose RGB fields
    /// fit in the low or high 3 bytes (the spare byte is dropped on the wire),
    /// otherwise equal to `dest_depth`.
    pub(crate) cpixel_depth: usize,
    /// When `cpixel_depth == 3`, whether the RGB fields sit in the high 3 bytes
    /// (drop the least-significant byte) rather than the low 3 (drop the most-
    /// significant byte).
    pub(crate) cpixel_high: bool,
}

impl PixelConversion {
    pub(crate) fn from_format(fmt: &rfb::PixelFormat) -> Self {
        let dest_depth = fmt.bits_per_pixel as usize / 8;
        // Derive bit width from leading_zeros, not count_ones: count_ones is
        // wrong for non-conforming max values. bit_width = 16 - lz on a u16.
        // Guard against max=0, which would underflow the shift.
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
        // A ZRLE CPIXEL is 3 bytes (not 4) when the format is 32bpp true-colour
        // with depth <= 24 and every RGB field sits within either the low 3 bytes
        // (drop the most-significant byte) or the high 3 bytes (drop the least-
        // significant byte). Depth 32 keeps all four bytes significant and uses a
        // full 4-byte CPIXEL.
        let (cpixel_depth, cpixel_high) = if dest_depth == 4 && fmt.depth <= 24 {
            let max_bit = (fmt.red_shift as u32 + red_bits)
                .max(fmt.green_shift as u32 + green_bits)
                .max(fmt.blue_shift as u32 + blue_bits);
            let min_bit = fmt.red_shift.min(fmt.green_shift).min(fmt.blue_shift) as u32;
            if max_bit <= 24 {
                (3, false)
            } else if min_bit >= 8 {
                (3, true)
            } else {
                (4, false)
            }
        } else {
            (dest_depth, false)
        };
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
            cpixel_depth,
            cpixel_high,
        }
    }

    /// Convert one internal 0x00RRGGBB pixel to the client's packed pixel value.
    #[inline]
    fn convert_one(&self, p: u32) -> u32 {
        let (r, g, b) = (p & 0xff0000, p & 0xff00, p & 0xff);
        r >> self.shift_r << self.out_shift_r
            | g >> self.shift_g << self.out_shift_g
            | b >> self.shift_b << self.out_shift_b
    }

    /// Append the ZRLE CPIXEL form of one internal 0x00RRGGBB pixel to `out`.
    /// For 32bpp true-colour formats this emits 3 bytes (the unused byte
    /// dropped, in the client's byte order); otherwise the full `dest_depth`
    /// bytes.
    pub(crate) fn push_cpixel(&self, src: u32, out: &mut Vec<u8>) {
        let p2 = self.convert_one(src);
        match self.cpixel_depth {
            3 => {
                // Drop the padding byte; emit the three significant bytes in the
                // client's byte order.
                let three: [u8; 3] = match (self.big_endian, self.cpixel_high) {
                    (false, false) => {
                        let x = p2.to_le_bytes();
                        [x[0], x[1], x[2]]
                    }
                    (false, true) => {
                        let x = p2.to_le_bytes();
                        [x[1], x[2], x[3]]
                    }
                    (true, false) => {
                        let x = p2.to_be_bytes();
                        [x[1], x[2], x[3]]
                    }
                    (true, true) => {
                        let x = p2.to_be_bytes();
                        [x[0], x[1], x[2]]
                    }
                };
                out.extend_from_slice(&three);
            }
            2 if self.big_endian => out.extend_from_slice(&(p2 as u16).to_be_bytes()),
            2 => out.extend_from_slice(&(p2 as u16).to_le_bytes()),
            1 => out.push(p2 as u8),
            4 if self.big_endian => out.extend_from_slice(&p2.to_be_bytes()),
            4 => out.extend_from_slice(&p2.to_le_bytes()),
            _ => unreachable!(),
        }
    }
}

/// Convert source pixels (0x00RRGGBB layout) to the client's negotiated
/// pixel format and append the result to `out`.
pub(crate) fn convert_pixels(src: &[u32], pc: &PixelConversion, out: &mut Vec<u8>) {
    if pc.no_convert {
        out.extend_from_slice(src.as_bytes());
        return;
    }

    for &p in src {
        let p2 = pc.convert_one(p);
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn cpixel_32bpp_drops_padding_byte() {
        // Standard 32bpp R@16 G@8 B@0 LE: RGB in the low 3 bytes, so a CPIXEL is
        // 3 bytes [B, G, R] (the zero MSB dropped).
        let pc = default_pc();
        assert_eq!(pc.cpixel_depth, 3);
        assert!(!pc.cpixel_high);
        let mut out = Vec::new();
        pc.push_cpixel(0x00FF0000, &mut out); // pure red
        assert_eq!(out, [0x00, 0x00, 0xFF]);
        out.clear();
        pc.push_cpixel(0x000000FF, &mut out); // pure blue
        assert_eq!(out, [0xFF, 0x00, 0x00]);
        out.clear();
        pc.push_cpixel(0x00112233, &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11]);
    }

    #[test]
    fn cpixel_high_three_bytes_drops_lsb() {
        // RGB packed in the high 3 bytes (R@24 G@16 B@8): CPIXEL drops the LSB.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 255.into(),
            green_max: 255.into(),
            blue_max: 255.into(),
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
            padding: [0; 3],
        });
        assert_eq!(pc.cpixel_depth, 3);
        assert!(pc.cpixel_high);
        let mut out = Vec::new();
        pc.push_cpixel(0x00FF0000, &mut out); // pure red -> high byte
        assert_eq!(out, [0x00, 0x00, 0xFF]);
    }

    #[test]
    fn cpixel_16bpp_is_two_bytes() {
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
        assert_eq!(pc.cpixel_depth, 2);
        let mut out = Vec::new();
        pc.push_cpixel(0x00FF0000, &mut out); // red -> 0xF800
        assert_eq!(out, 0xF800u16.to_le_bytes());
    }

    #[test]
    fn cpixel_8bpp_is_one_byte() {
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 8,
            depth: 8,
            big_endian_flag: 0,
            true_color_flag: 1,
            red_max: 7.into(),
            green_max: 7.into(),
            blue_max: 3.into(),
            red_shift: 5,
            green_shift: 2,
            blue_shift: 0,
            padding: [0; 3],
        });
        assert_eq!(pc.cpixel_depth, 1);
        let mut out = Vec::new();
        pc.push_cpixel(0x00FFFFFF, &mut out); // white -> 0xFF
        assert_eq!(out, [0xFF]);
    }

    #[test]
    fn cpixel_big_endian_low_three_bytes() {
        // 32bpp R@16 G@8 B@0 with the big-endian flag set: RGB still in the low
        // 3 bytes, but emitted most-significant byte first.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 1,
            true_color_flag: 1,
            red_max: 255.into(),
            green_max: 255.into(),
            blue_max: 255.into(),
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            padding: [0; 3],
        });
        assert_eq!(pc.cpixel_depth, 3);
        assert!(!pc.cpixel_high);
        assert!(pc.big_endian);
        let mut out = Vec::new();
        pc.push_cpixel(0x00112233, &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33]); // [R, G, B]
        out.clear();
        pc.push_cpixel(0x00FF0000, &mut out); // pure red
        assert_eq!(out, [0xFF, 0x00, 0x00]);
    }

    #[test]
    fn cpixel_big_endian_high_three_bytes() {
        // Big-endian with RGB packed in the high 3 bytes (R@24 G@16 B@8): the
        // padding byte is the least-significant one, dropped before the BE bytes.
        let pc = PixelConversion::from_format(&rfb::PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian_flag: 1,
            true_color_flag: 1,
            red_max: 255.into(),
            green_max: 255.into(),
            blue_max: 255.into(),
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
            padding: [0; 3],
        });
        assert_eq!(pc.cpixel_depth, 3);
        assert!(pc.cpixel_high);
        assert!(pc.big_endian);
        let mut out = Vec::new();
        pc.push_cpixel(0x00112233, &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33]); // [R, G, B] from the high 3 bytes
    }
}
