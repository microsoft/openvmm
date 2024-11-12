// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Non-linear blitting routines.

use crate::text_mode::Parameters;
use guestmem::GuestMemory;
use std::cmp::max;
use std::cmp::min;

pub fn blit_non_linear4to32(
    vram: &GuestMemory,
    parameters: &Parameters,
    colors: &[u32; 256],
    pixel_pan: u8,
    double_horiz: bool,
    double_vert: bool,
    src_start: usize,
    line_offset_pixels: u16,
    dest_start: usize,
    dest_row_bytes: usize,
) {
    let horizontal_load_mask = if double_horiz { 0xF } else { 0x7 };
    let horizontal_advance_mask = double_horiz as i32;
    let horizontal_shift = if double_horiz { 4 } else { 3 };
    let vertical_shift = double_vert as u8;
    let source_row_bytes = line_offset_pixels;

    let pixel_pan = (pixel_pan as i32) << horizontal_advance_mask;

    for row_iter in parameters.row_number_top..parameters.row_number_bottom {
        let mut left_col = max(parameters.clip_left, parameters.column_left);
        let right_col = min(parameters.clip_right, parameters.column_right);

        if left_col >= right_col {
            continue;
        }

        let mut offset = (row_iter * dest_row_bytes as i32) + left_col * 4;
        let mut write_32_bit = |n: u32| {
            vram.write_at((dest_start + offset as usize) as u64, &n.to_ne_bytes())
                .unwrap();
            offset += 4;
        };

        let source_video_offset = source_row_bytes as i32 * (row_iter >> vertical_shift);

        let read_u32 = |offset: i32| -> u32 {
            vram.read_plain(src_start as u64 + (source_video_offset as u64 + offset as u64) * 4)
                .unwrap()
        };

        let mut non_linear_data = 0;
        if ((left_col + pixel_pan) & horizontal_load_mask) != 0 {
            let converted_source_address0 = read_u32((left_col + pixel_pan) >> horizontal_shift);
            non_linear_data = converted_source_address0;

            let shift_alignment =
                ((left_col + pixel_pan) & horizontal_load_mask) >> horizontal_advance_mask;

            non_linear_data <<= shift_alignment;
        }

        while left_col < right_col {
            if ((left_col + pixel_pan) & horizontal_load_mask) == 0 {
                let converted_source_address0 =
                    read_u32((left_col + pixel_pan) >> horizontal_shift);
                non_linear_data = converted_source_address0;
            }

            let index = ((non_linear_data >> 28) & 0x08)
                | ((non_linear_data >> 21) & 0x04)
                | ((non_linear_data >> 14) & 0x02)
                | ((non_linear_data >> 7) & 0x01);

            let source0 = colors[index as usize];
            write_32_bit(source0);

            left_col += 1;
            if ((left_col + pixel_pan) & horizontal_advance_mask) == 0 {
                non_linear_data <<= 1;
            }
        }
    }
}
