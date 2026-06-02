// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pixel format conversion: cached conversion parameters and the per-pixel
//! conversion routine that emits client-formatted bytes from internal
//! 0x00RRGGBB pixels.

use crate::rfb;
use zerocopy::IntoBytes;

/// Pre-computed pixel conversion parameters, cached per-connection to avoid
/// recomputing from the PixelFormat on every `convert_pixels` call.
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
}

impl PixelConversion {
    pub(crate) fn from_format(fmt: &rfb::PixelFormat) -> Self {
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
pub(crate) fn convert_pixels(src: &[u32], pc: &PixelConversion, out: &mut Vec<u8>) {
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
}
