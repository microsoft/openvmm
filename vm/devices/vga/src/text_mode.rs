// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::emu::TextModeState;
use crate::spec;
use guestmem::GuestMemory;
use std::cmp::min;

struct ExpandPixelsTable([[u32; 4]; 16]);

pub struct Parameters {
    pub column_left: i32,
    pub column_right: i32,
    pub clip_left: i32,
    pub clip_right: i32,
    pub row_number_top: i32,
    pub row_number_bottom: i32,
}

pub fn blit_text(
    vram: &GuestMemory,
    parameters: &Parameters,
    text_state: &TextModeState,
    colors: &[u32; 256],
    text_start: usize,
    dest_start: usize,
) {
    let (cursor_row, cursor_col);

    cursor_row = text_state.cursor_row;
    cursor_col = text_state.cursor_col;

    for row_iter in parameters.row_number_top..parameters.row_number_bottom {
        let mut old_color = 256; // this value will force the expand table to be updated first time.
        let mut expand_pixels_table = ExpandPixelsTable([[0; 4]; 16]);

        const TEXT_DOUBLE_TABLE: [u8; 16] = [
            0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3,
            0xFC, 0xFF,
        ];

        let char_bit_map_base = text_state.char_set_1 as u64 * 0x8000 + 2;
        let ext_char_bit_map_base = text_state.char_set_2 as u64 * 0x8000 + 2;

        // calculate which text row we're going to be drawing.
        let text_row = (row_iter / text_state.text_char_height as i32) as u16;
        let char_row = (row_iter % text_state.text_char_height as i32) as u16;

        let is_cursor_row = text_state.draw_text_cursor
            && text_state.cursor_blink_state
            && (text_row == cursor_row);

        let char_width = if text_state.lo_res_text_mode {
            spec::VGA_LORES_CHARACTER_WIDTH
        } else {
            spec::VGA_HIRES_CHARACTER_WIDTH
        } as i32;

        // Try to clip to full characters.
        let mut left_col = (parameters.column_left & !(char_width - 1)).max(parameters.clip_left);
        let right_col = ((parameters.column_right + (char_width - 1)) & !(char_width - 1))
            .min(parameters.clip_right);

        while left_col < right_col {
            let cell =
                text_row * text_state.current_text_columns + (left_col as u16 / char_width as u16);

            // Read and translate the value.
            let text_data = vram
                .read_plain(text_start as u64 + cell as u64 * 8)
                .expect("BUGBUG");

            let mut text_data = u16::from_be_bytes(text_data);
            if text_state.blinking_enabled {
                if text_state.blinking_state && text_data & spec::CGA_CHARACTER_BLINKING_MASK != 0 {
                    text_data = (text_data & !spec::CGA_CHARACTER_FOREGROUND_COLOR_MASK)
                        | ((text_data
                            & spec::CGA_CHARACTER_BACKGROUND_COLOR_MASK
                            & !spec::CGA_CHARACTER_BLINKING_MASK)
                            >> 4);
                }
                text_data &= 0xff7f;
            }

            if old_color
                != (text_data
                    & (spec::CGA_CHARACTER_BACKGROUND_COLOR_MASK
                        | spec::CGA_CHARACTER_FOREGROUND_COLOR_MASK))
            {
                old_color = text_data
                    & (spec::CGA_CHARACTER_BACKGROUND_COLOR_MASK
                        | spec::CGA_CHARACTER_FOREGROUND_COLOR_MASK);

                let fore_color = text_data as usize & 15;
                let back_color = (text_data >> 4) as usize & 15;
                rebuild_expand_pixels_table(
                    &mut expand_pixels_table,
                    colors[fore_color],
                    colors[back_color],
                );
            }

            let char_data: u8;
            if is_cursor_row
                && ((left_col / char_width) == cursor_col as i32)
                && (char_row >= text_state.cursor_first_scanline)
                && (char_row <= text_state.cursor_last_scanline)
            {
                char_data = 0xFF;
            } else {
                let mut current_char_bit_map =
                    char_bit_map_base + (((text_data as u64 >> 8) << 7) + 4 * char_row as u64);
                if text_state.character_set_512 {
                    if (text_data & 0x08) == 0 {
                        current_char_bit_map = ext_char_bit_map_base
                            + (((text_data as u64 >> 8) << 7) + 4 * char_row as u64);
                    }
                }

                char_data = vram.read_plain(current_char_bit_map).unwrap();
            }

            let mut char_data = char_data as u16;
            if text_state.lo_res_text_mode {
                // pixel expand.
                char_data = TEXT_DOUBLE_TABLE[char_data as usize & 0xF] as u16
                    | ((TEXT_DOUBLE_TABLE[char_data as usize >> 4] as u16) << 8);
            }

            let mut offset = (row_iter * 640 + left_col) * 4;
            let mut write_32bit = |n: u32| {
                vram.write_at((dest_start + offset as usize) as u64, &n.to_ne_bytes())
                    .unwrap();
                offset += 4;
            };

            // there are three possible cases here.
            // 1. We have a partial char
            // 3. We have a full, aligned char
            // 4. we have a full, misaligned char.
            if (left_col & (char_width - 1)) != 0 || (right_col - left_col < char_width) {
                let end_blit = min(right_col, (left_col + char_width) & !(char_width - 1));

                let fore_color = text_data as usize & 15;
                let fore_color = colors[fore_color];
                let back_color = (text_data as usize >> 4) & 15;
                let back_color = colors[back_color];

                while left_col < end_blit {
                    let test_bit = char_width - (left_col & (char_width - 1)) - 1;
                    if ((char_data >> test_bit) & 0x1) != 0 {
                        write_32bit(fore_color);
                    } else {
                        write_32bit(back_color);
                    }

                    left_col += 1;
                }
            } else if right_col - left_col >= char_width {
                left_col += char_width;

                let mut fill_char_data = if text_state.lo_res_text_mode {
                    char_data >> 8
                } else {
                    char_data
                } as u8;

                for _ in (0..char_width).step_by(spec::VGA_HIRES_CHARACTER_WIDTH.into()) {
                    write_32bit(expand_pixels_table.0[fill_char_data as usize >> 4][0]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize >> 4][1]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize >> 4][2]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize >> 4][3]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize & 0xF][0]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize & 0xF][1]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize & 0xF][2]);
                    write_32bit(expand_pixels_table.0[fill_char_data as usize & 0xF][3]);

                    // get the second half for a lo-res pixel.
                    fill_char_data = char_data as u8;
                }
            }
        }
    }
}

fn rebuild_expand_pixels_table(
    expand_table: &mut ExpandPixelsTable,
    fore_color: u32,
    back_color: u32,
) {
    for cur_entry in 0..16 {
        let color = |mask| {
            if cur_entry & mask != 0 {
                fore_color
            } else {
                back_color
            }
        };
        expand_table.0[cur_entry] = [color(8), color(4), color(2), color(1)];
    }
}
