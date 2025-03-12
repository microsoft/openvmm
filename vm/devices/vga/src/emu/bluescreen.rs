// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for changing modes to draw a blue screen when Windows crashes.
//!
//! This is a bit weird--instead of directly manipulating the VGA state, this
//! code manipulates the VGA registers in the way the SVGA BIOS would. This is
//! done for historical reasons.

use super::Emulator;
use crate::spec;
use crate::spec::CrtControlReg;

const VGA_CLK_VALUE: u8 = 0; // (CRT index 0x5c) & 0x30
const POST_VFB_BIT: u8 = 0; // (CRT index 0x5c) & 0x40
const POST_CR65_VALUE: u8 = 0; // (CRT index 0x65)

impl Emulator {
    fn emulate_in(&mut self, port: u16) -> u8 {
        self.io_port_read(port, 1) as u8
    }

    fn emulate_out(&mut self, port: u16, value: u8) {
        self.io_port_write(port, 1, value.into());
    }

    fn emulate_double_out(&mut self, port: u16, index: u8, value: u8) {
        self.io_port_write(port, 2, ((value as u32) << 8) | index as u32);
    }

    fn emulate_masked_double_out(&mut self, port: u16, index: u8, value: u8, mask: u8) {
        self.emulate_out(port, index);
        let value = (self.emulate_in(port + 1) & mask) | value;
        self.emulate_out(port + 1, value);
    }

    fn get_crt_port(&mut self) -> u16 {
        let data = self.emulate_in(0x3CC);

        if data & 1 != 0 { 0x3D4 } else { 0x3B4 }
    }

    fn turn_screen_on_off(&mut self, on: bool) {
        self.emulate_out(0x3C4, 1);
        let mut data = self.emulate_in(0x3C5);

        if on {
            data &= !0x20;
        } else {
            data |= 0x20;
        }

        self.emulate_out(0x3C5, data);
    }

    fn access_afc(&mut self, crt_port: u16, value: u8) {
        self.emulate_out(crt_port, 0x40);
        let save = self.emulate_in(crt_port + 1);
        let data = save | 1;
        self.emulate_out(crt_port + 1, data);
        self.emulate_out(0x4AE8, value);
        self.emulate_double_out(crt_port, 0x40, save);
    }

    fn reset_dcc(&mut self, crt_port: u16) {
        const DEFAULT_CRTC: &[u16] = &[
            0x08531, // Index 31h - Memory Control.
            0x00050, // Index 50h - Extended System Cont 1
            0x00051, // Index 51h - Extended System Cont 2
            0x03854, // Index 54h - Extended Memory Cont 2
            0x00055, // Index 55h - Extended DAC Control
            0x0C058, // Index 58h - Linear Address Window Control
            0x0805C, // Index 5Ch - GOP Control
            0x0005D, // Index 5Dh - Extended Horizontal Overflow
            0x0005E, // Index 5Eh - Extended Vertical Overflow
            0x00760, // Index 60h - Extended Memory Cont 3
            0x08061, // Index 61h - Extended Memory Cont 4
            0x0A162, // Index 62h - Extended Memory Cont 5
            0x00063, // Index 63h - Extended Sync 3
            0x00064, // Index 64h - Extended Sync 4
            0x00065, // Index 65h - Extended Miscellaneous
            0x00032, // Index 32h - Backward Compat 1
            0x00033, // Index 33h - Backward Compat 2
            0x00034, // Index 34h - Backward Compat 3
            0x00035, // Index 35h - CRTC Lock
            0x0053A, // Index 3Ah - S3 Misc 1
            0x05A3B, // Index 3Bh - Data Transfer Exec Pos
            0x0103C, // Index 3Ch - Interlace Retrace start
            0x00043, // Index 43h - Extended Mode
            0x05840, // Index 40h - System Configuration
            0x00042, // Index 42h - Mode Control
        ];

        self.access_afc(crt_port, 0x02);

        for &value in DEFAULT_CRTC {
            let register_index = value as u8;

            let mut data = (value >> 8) as u8;

            self.emulate_out(crt_port, register_index);

            match register_index {
                0x31 => {
                    data |= self.emulate_in(crt_port + 1) & 0xb5;
                }

                0x32 => {
                    data |= self.emulate_in(crt_port + 1) & 0x40;
                }

                0x3A => {
                    data |= self.emulate_in(crt_port + 1) & 0x88;
                }

                0x40 => {
                    //
                    // The line below removes the high order nibble from the table value at
                    // runtime instead of having the correct value in the table.  It is left
                    // this way to keep the table consistent with the table in the bios.
                    //
                    data = self.emulate_in(crt_port + 1) & 0xfe | (data & 0x0f);
                }

                0x42 => {
                    data |= self.emulate_in(crt_port + 1) & 0xdf;
                }

                0x58 => {
                    data |= self.emulate_in(crt_port + 1) & 0xcc;
                }

                0x5C => {
                    data |= self.emulate_in(crt_port + 1) & 0x0f | VGA_CLK_VALUE | POST_VFB_BIT;
                }

                0x65 => {
                    data = POST_CR65_VALUE;
                }

                _ => {
                    // The index does not need a special mask, just use the table value data.
                }
            }

            self.emulate_out(crt_port + 1, data);
        }
    }

    fn load_regs(
        &mut self,
        port: u16,
        registers: &mut &[u8],
        num_registers: usize,
        initial_index: u8,
    ) {
        for (i, &register) in registers[..num_registers].iter().enumerate() {
            self.emulate_double_out(port, initial_index + i as u8, register);
        }
        *registers = &registers[..num_registers];
    }

    fn program_sequencer(&mut self, crt_port: u16, registers: &mut &[u8]) {
        self.emulate_in(crt_port + 6);
        self.emulate_out(0x3C0, 0);
        self.emulate_double_out(0x3C4, 0x01, 0x20);
        self.emulate_double_out(0x3C4, 0x01, registers[0]);
        *registers = &registers[1..];
        self.load_regs(crt_port, registers, 2, 0x02);
    }

    fn program_misc(&mut self, crt_port: u16, registers: &mut &[u8]) -> u16 {
        self.emulate_masked_double_out(crt_port, 0x5C, 0, 0xCF);

        self.emulate_double_out(crt_port, 0x42, 0);

        self.emulate_double_out(crt_port, 0x11, 0);
        self.emulate_double_out(crt_port, 0x00, 0x5F);

        let data = registers[0];
        *registers = &registers[1..];
        self.emulate_out(0x3C2, data);
        if data & 1 != 0 { 0x3DA } else { 0x3BA }
    }

    fn program_attribute(&mut self, input_status1_port: u16, registers: &mut &[u8]) {
        self.emulate_in(input_status1_port);

        for index in 0..0x14 {
            self.emulate_out(0x3C0, index);
            self.emulate_out(0x3C0, registers[0]);
            *registers = &registers[1..];
        }

        self.emulate_out(0x3C0, 0x20);
    }

    fn program_graphics(&mut self, registers: &mut &[u8]) {
        self.load_regs(0x3CE, registers, 9, 0x00);
    }

    fn program_crtc(&mut self, crt_port: u16, registers: &mut &[u8]) {
        self.emulate_masked_double_out(crt_port, 0x11, 0, 0x7F);

        self.load_regs(crt_port, registers, 0x19, 0);
    }

    fn minimal_set_scanline_length(&mut self, crt_port: u16, mut scanline_length: u16) {
        // This halving is required.  CalculateLineOffsetPixels in turn doubles
        // the value when it gets it.
        scanline_length /= 2;

        let high_bits = ((scanline_length >> 8) & 0xff) << 4;

        self.emulate_double_out(crt_port, 0x13, scanline_length as u8);
        self.emulate_masked_double_out(crt_port, 0x51, high_bits as u8, 0xCF);

        if (high_bits & 0x30) == 0 {
            self.emulate_masked_double_out(crt_port, 0x43, 0, 0xFB);
        }
    }

    fn program_ecrtc(&mut self, crt_port: u16, registers: &mut &[u8]) {
        const EXT_CRTC_INDEXES: &[u8] = &[
            0x42, // Mode Control
            0x3B, // Data Transfer Execute Position
            0x3C, // Interlace Retrace Start
            0x31, // Memory Configuration
            0x3A, // Miscellaneous 1
            0x40, // System Configuration
            0x50, // Extended System Control 1
            0x54, // Extended Memory Control 2
            0x5D, // Extended Horizontal Overflow
            0x60, // Extended Memory Control 3
            0x61, // Extended Memory Control 4
            0x62, // Extended Memory Control 5
            0x58, // Linear Address Window Control
            0x33, // Backward Compatibility 2
            0x43, // Extended Mode
            0x13, // Offset
            0x5E, // Extended Vertical Overflow
            0x51, // Extended System Control 2
            0x5C, // General Output Port
            0x34, // Backward Compatibility 3
        ];

        let mut is_vga = false;

        for (&register_index, &data) in EXT_CRTC_INDEXES.iter().zip(*registers) {
            self.emulate_out(crt_port, register_index);

            let mut data = data;
            match register_index {
                0x31 => {
                    data |= self.emulate_in(crt_port + 1) & 0xcf;
                    if (data & 0x08) != 0 {
                        is_vga = true;
                    }
                }

                0x3A => {
                    data |= self.emulate_in(crt_port + 1) & 0x80;
                }

                0x40 => {
                    data |= self.emulate_in(crt_port + 1) & 0xfe;
                }

                0x58 => {
                    data |= self.emulate_in(crt_port + 1) & 0xcc;
                }

                0x5C => {
                    // SHORTCUT:  We use a priori knowledge that this is 640x480 to
                    // OR in the POST_VFB_Bit (others sizes don't do that).
                    data &= 0xbf;
                    data |= POST_VFB_BIT;
                }

                _ => {
                    // The index does not need a special mask, just use the table value data.
                }
            }

            self.emulate_out(crt_port + 1, data);
        }

        // Move Registers forward as the calling function expects
        *registers = &registers[EXT_CRTC_INDEXES.len()..];

        if is_vga {
            // SHORTCUT:  Using a prior knowledge that the scanline length should be 640.
            self.minimal_set_scanline_length(crt_port, 640);
        }
    }

    fn setup_dac(&mut self, crt_port: u16) {
        // SHORTCUT:  We use a priori knowledge that the mode is 32bpp to pick the
        // right data value of 0xD0.  The value comes from S3_DAC.S3_32_VALUE.
        self.emulate_masked_double_out(crt_port, 0x67, 0xD0, 0x0f);
    }

    /// Switches mode to a linear one, so that blue screen doesn't have to rely on
    /// unreasonably slow 4-bit planar.  The mode we're going to is actually bios
    /// mode 0x112:  640x480, 32 bit.  Because the synthvid vsc can't invoke the
    /// bios itself, we achieve the mode by duplicating what the bios would have
    /// done.
    ///
    /// The vsc has a priori knowledge about the mode we're going to switch to,
    /// so don't change this here without updating it there.
    pub(super) fn do_blue_screen_mode_change(&mut self) {
        // We are implementing mode 112:  640x480, 32bpp.  For reference,
        // here's what the original VPM looks like for it:
        //
        // Mode_112h       label byte                 ; VESA 112h, 640 x 480 x 32
        //         db      070h                       ; Internal Mode Number
        //         dw      OFFSET Mode_640x480_Table  ; Index to register values
        //         db      00Eh                       ; Memory Mode Control (Seq. 04h)
        //         db      003h                       ; Advanced Function Control (4AE8h)
        //
        //         dw      OFFSET  CRTC_70_10
        //         dw      OFFSET ECRTC_70_10
        //
        // Thus come the next two constants:
        const MEMORY_MODE: u8 = 0x00E;
        const ADVANCED_CONTROL: u8 = 0x003;

        //
        // The following table is just the registers section of the bios Mode_640x480_Table.
        //
        const MODE_640_480_TABLE_REGISTERS: &[u8] = &[
            0x021, 0x00F, 0x000, // Sequencer (Index 01h-03h)
            0x0EF, // Misc. output
            // Attribute Controller                          Index
            0x000, 0x001, 0x002, 0x003, 0x004, 0x005, // 00h-05h
            0x006, 0x007, 0x010, 0x011, 0x012, 0x013, // 06h-0Bh
            0x014, 0x015, 0x016, 0x017, 0x041, 0x000, // 0Ch-11h
            0x00F, 0x000, // 12h-13h
            // Graphics Controller                           Index
            0x000, 0x000, 0x000, 0x000, 0x000, 0x040, // 00h-05h
            0x005, 0x00F, 0x0FF, // 06h-08h
        ];

        const CRTC_70_10: &[u8] = &[
            0x090, 0x04F, 0x078, 0x091, 0x07B, 0x00E, // 00h-05h
            0x00A, 0x01A, 0x000, 0x040, 0x000, 0x000, // 06h-0Bh
            0x000, 0x000, 0x0FF, 0x000, 0x0EA, 0x00C, // 0Ch-11h
            0x0DF, 0x000, 0x060, 0x0DF, 0x003, 0x0AB, // 12h-17h
            0x0FF, // 18h
        ];

        const ECRTC_70_10: &[u8] = &[
            0x004, 0x092, 0x040, 0x08B, 0x015, // Index: 42h, 3Bh, 3Ch, 31h, 3Ah
            0x000, 0x070, 0x018, 0x000, 0x02F, // Index: 40h, 50h, 54h, 5Dh, 60h
            0x081, 0x0E0, 0x000, 0x000, 0x000, // Index: 61h, 62h, 58h, 33h, 43h
            0x040, 0x040, 0x010, 0x0A0, 0x000, // Index: 13h, 5Eh, 51h, 5Ch, 34h
        ];

        let crt_port = self.get_crt_port();

        self.turn_screen_on_off(false);

        // S3_unlock
        self.emulate_double_out(crt_port, 0x38, 0x84);
        self.emulate_double_out(crt_port, 0x39, 0x40);

        self.reset_dcc(crt_port);

        //
        // Clear the screen.
        //
        self.emulate_double_out(
            crt_port,
            CrtControlReg::CUSTOM_VS_GENERAL_EXTENSION_REGISTER.0,
            spec::BIOS_CLEAR_SCREEN_CODE,
        );

        self.emulate_double_out(0x3C4, 0x04, MEMORY_MODE);

        self.access_afc(crt_port, ADVANCED_CONTROL);

        let mut registers = MODE_640_480_TABLE_REGISTERS;

        self.program_sequencer(crt_port, &mut registers);
        let input_status_register1 = self.program_misc(crt_port, &mut registers);
        self.program_attribute(input_status_register1, &mut registers);
        self.program_graphics(&mut registers);

        self.program_crtc(crt_port, &mut &CRTC_70_10[..]);
        self.program_ecrtc(crt_port, &mut &ECRTC_70_10[..]);
        self.setup_dac(crt_port);

        // SHORTCUT:  With a priori knowledge that the MemoryModel is DirectColor, we can skip
        // Load_Dac_palette entirely, since it's a no-op for DirectColor.

        self.turn_screen_on_off(true);

        //
        // Turn linear mode on (a prior we know we want on, not off)
        //
        self.emulate_masked_double_out(0x3D4, 0x58, 0x13, 0xEC);
    }
}
