// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]

use inspect::Inspect;
use open_enum::open_enum;

pub const PCI_VENDOR_ID: u16 = 0x1414;
pub const PCI_DEVICE_ID: u16 = 0x5353;
pub const PCI_REVISION: u8 = 0;
pub const PCI_SUBSYSTEM: u8 = 0;
pub const PCI_VIDEO_CLASS_CODE: u8 = 3;
pub const S3_TRIO_CHIPSET_REV_NUMBER: u8 = 0;

pub const MDA_HARDWARE_PORT_RANGE: u16 = 0x3B0;
pub const VGA_HARDWARE_PORT_RANGE: u16 = 0x3C0;
pub const CGA_HARDWARE_PORT_RANGE: u16 = 0x3D0;

pub const MDA_INDEX_REGISTER_PORT: u16 = 0x3B4; // write-only
pub const MDA_DATA_REGISTER_PORT: u16 = 0x3B5; // read/write
pub const MDA_MODE_CONTROL_REGISTER_PORT: u16 = 0x3B8; // write-only
pub const MDA_INPUT_STATUS_REG1_PORT: u16 = 0x3BA; // read-only

pub const CGA_INDEX_REGISTER_PORT: u16 = 0x3D4; // read/write
pub const CGA_DATA_REGISTER_PORT: u16 = 0x3D5; // read/write
pub const CGA_MODE_CONTROL_REGISTER_PORT: u16 = 0x3D8; // read/write (not supported in VGA)
pub const CGA_INPUT_STATUS_REG1_PORT: u16 = 0x3DA; // read-only
pub const CGA_UNKNOWN_PORT_3D3: u16 = 0x3D3;
pub const CGA_UNKNOWN_PORT_3D6: u16 = 0x3D6;
pub const CGA_UNKNOWN_PORT_3D7: u16 = 0x3D7;
pub const CGA_UNKNOWN_PORT_3DB: u16 = 0x3DB;
pub const CGA_UNKNOWN_PORT_3DC: u16 = 0x3DC;
pub const CGA_UNKNOWN_PORT_3DD: u16 = 0x3DD;
pub const CGA_UNKNOWN_PORT_3DE: u16 = 0x3DE;
pub const CGA_UNKNOWN_PORT_3DF: u16 = 0x3DF;

pub const S3_ADV_FUNCTION_CONTROL_PORT: u16 = 0x4AE8; // read/write

open_enum! {
    pub enum VgaPort: u16 {
        INDEX_DATA_REG_ATTR_PORT        = 0x3C0,   // write-only
        ATTRIBUTE_READ_PORT             = 0x3C1,   // read-only
        INPUT_STATUS_REG0_PORT          = 0x3C2,   // read-only
        MISC_OUTPUT_WRITE_PORT          = 0x3C2,   // write-only
        SUBSYSTEM_ENABLE_PORT           = 0x3C3,   // read/write (used only by IBM - we ignore)
        SEQ_INDEX_REGISTER_PORT         = 0x3C4,   // read/write
        SEQ_DATA_REGISTER_PORT          = 0x3C5,   // read/write
        PEL_MASK_REGISTER_PORT          = 0x3C6,   // read/write
        PEL_ADDRESS_READ_REGISTER_PORT  = 0x3C7,   // write-only
        DAC_STATUS_REGISTER_PORT        = 0x3C7,   // read-only
        PEL_ADDRESS_WRITE_REGISTER_PORT = 0x3C8,   // read/write
        PEL_DATA_REGISTER_PORT          = 0x3C9,   // read/write
        FEATURE_CONTROL_REG_PORT        = 0x3CA,   // read-only
        GRAPHICS_POS_REGISTER2_PORT     = 0x3CA,   // write-only (not supported in VGA - EGA only)
        UNKNOWN_PORT_3CB                = 0x3CB,   // unsupported by us - not documented
        GRAPHICS_POS_REGISTER1_PORT     = 0x3CC,   // write-only (not supported in VGA - EGA only)
        MISC_OUTPUT_READ_PORT           = 0x3CC,   // read-only
        UNKNOWN_PORT_3CD                = 0x3CD,   // unsupported by us - not documented
        GRAPHICS_INDEX_REG_PORT         = 0x3CE,   // read/write
        GRAPHICS_DATA_REG_PORT          = 0x3CF,   // read/write
    }
}

open_enum::open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum CrtControlReg: u8 {
        HORIZONTAL_TOTAL_REGISTER                       = 0x00,     // completely ignored by us
        HORIZONTAL_DISPLAY_END_REGISTER                 = 0x01,     // completely ignored by us
        START_HORIZONTAL_BLANK_REGISTER                 = 0x02,     // completely ignored by us
        END_HORIZONTAL_BLANK_REGISTER                   = 0x03,     // completely ignored by us
        START_HORIZONTAL_RETRACE_REGISTER               = 0x04,     // completely ignored by us
        END_HORIZONTAL_RETRACE_REGISTER                 = 0x05,     // completely ignored by us
        VERTICAL_TOTAL_REGISTER                         = 0x06,     // completely ignored by us
        OVERFLOW_REGISTER                               = 0x07,
        PRESET_ROW_SCAN_REGISTER                        = 0x08,     // completely ignored by us
        MAX_SCANLINE_REGISTER                           = 0x09,
        CURSOR_START_REGISTER                           = 0x0A,
        CURSOR_END_REGISTER                             = 0x0B,
        START_ADDRESS_HI_REGISTER                       = 0x0C,
        START_ADDRESS_LO_REGISTER                       = 0x0D,
        CURSOR_LOCATION_HI_REGISTER                     = 0x0E,
        CURSOR_LOCATION_LO_REGISTER                     = 0x0F,
        VERTICAL_RETRACE_HI_REGISTER                    = 0x10,     // completely ignored by us
        VERTICAL_RETRACE_LO_REGISTER                    = 0x11,
        VERTICAL_DISPLAY_END_REGISTER                   = 0x12,
        OFFSET_REGISTER                                 = 0x13,
        UNDERLINE_LOCATION_REGISTER                     = 0x14,
        START_VERTICAL_BLANK_REGISTER                   = 0x15,
        END_VERTICAL_BLANK_REGISTER                     = 0x16,     // completely ignored by us
        MODE_CONTROL_REGISTER                           = 0x17,
        LINE_COMPARE_REGISTER                           = 0x18,

        UNSUPPORTED_22_REGISTER                         = 0x22,     // CPU Latch Data Register
        UNSUPPORTED_24_REGISTER                         = 0x24,     // Attribute Index Register

        S3_DEVICE_ID_HI_REGISTER                        = 0x2D,     // read-only
        S3_DEVICE_ID_LO_REGISTER                        = 0x2E,     // read-only
        S3_DEVICE_REVISION_REGISTER                     = 0x2F,     // read-only

        S3_CHIP_REVISION_NUMBER_REGISTER                = 0x30,     // read-only
        S3_MEMORY_CONFIGURATION_REGISTER                = 0x31,
        S3_BACKWARD_COMPATIBILITY_1_REGISTER            = 0x32,
        S3_BACKWARD_COMPATIBILITY_2_REGISTER            = 0x33,
        S3_BACKWARD_COMPATIBILITY_3_REGISTER            = 0x34,
        S3_REGISTER_LOCK_REGISTER                       = 0x35,
        S3_CONFIGURATION_1_REGISTER                     = 0x36,
        S3_CONFIGURATION_2_REGISTER                     = 0x37,
        S3_UNLOCK_VGA_REGISTERS_1_REGISTER              = 0x38,
        S3_UNLOCK_VGA_REGISTERS_2_REGISTER              = 0x39,
        S3_MISC_1_REGISTER                              = 0x3A,
        S3_DATA_TRANSFER_REGISTER                       = 0x3B,
        S3_INTERLACE_START_REGISTER                     = 0x3C,

        S3_SYSTEM_CONFIGURATION_REGISTER                = 0x40,
        S3_BIOS_FLAG_REGISTER                           = 0x41,
        S3_MODE_CONTROL_REGISTER                        = 0x42,
        S3_EXTENDED_MODE_REGISTER                       = 0x43,

        // The old S3 hardware graphics cursor is now dead functionality.
        S3_HW_CURSOR_DEAD_1                             = 0x45,
        S3_HW_CURSOR_DEAD_2                             = 0x46,
        S3_HW_CURSOR_DEAD_3                             = 0x47,
        S3_HW_CURSOR_DEAD_4                             = 0x48,
        S3_HW_CURSOR_DEAD_5                             = 0x49,
        S3_HW_CURSOR_DEAD_6                             = 0x4A,
        S3_HW_CURSOR_DEAD_7                             = 0x4B,
        S3_HW_CURSOR_DEAD_8                             = 0x4C,
        S3_HW_CURSOR_DEAD_9                             = 0x4D,
        S3_HW_CURSOR_DEAD_10                            = 0x4E,
        S3_HW_CURSOR_DEAD_11                            = 0x4F,

        S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER           = 0x50,
        S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER           = 0x51,
        S3_EXTENDED_BIOS_FLAG_1_REGISTER                = 0x52,
        S3_MMIO_DEAD_1                                  = 0x53,
        S3_EXTENDED_MEMORY_CONTROL_2_REGISTER           = 0x54,
        S3_EXTENDED_DAC_CONTROL_REGISTER                = 0x55,
        S3_EXTERNAL_SYNC_CONTROL_1_REGISTER             = 0x56,
        S3_EXTERNAL_SYNC_CONTROL_2_REGISTER             = 0x57,
        S3_LINEAR_ADDRESS_WINDOW_CONTROL_REGISTER       = 0x58,
        S3_LINEAR_ADDRESS_WINDOW_POSITION_1_REGISTER    = 0x59,
        S3_LINEAR_ADDRESS_WINDOW_POSITION_2_REGISTER    = 0x5A,
        S3_EXTENDED_BIOS_FLAG_2_REGISTER                = 0x5B,
        S3_GENERAL_OUTPUT_REGISTER                      = 0x5C,
        S3_EXTENDED_HORIZONTAL_OVERFLOW_REGISTER        = 0x5D,
        S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER          = 0x5E,
        S3_EXTENDED_BUS_GRANT_REGISTER                  = 0x5F,     // found no info on this one

        S3_EXTENDED_MEMORY_CONTROL_3_REGISTER           = 0x60,
        S3_EXTENDED_MEMORY_CONTROL_4_REGISTER           = 0x61,
        S3_EXTENDED_MEMORY_CONTROL_5_REGISTER           = 0x62,
        S3_EXTENDED_MISC_CONTROL_0_REGISTER             = 0x65,
        S3_EXTENDED_MISC_CONTROL_1_REGISTER             = 0x66,
        S3_EXTENDED_MISC_CONTROL_2_REGISTER             = 0x67,
        S3_CONFIGURATION_3_REGISTER                     = 0x68,
        S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER           = 0x69,
        S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER           = 0x6A,
        S3_EXTENDED_BIOS_FLAG_3_REGISTER                = 0x6B,
        S3_EXTENDED_BIOS_FLAG_4_REGISTER                = 0x6C,
        S3_EXTENDED_BIOS_FLAG_5_REGISTER                = 0x6D,
        S3_EXTENDED_BIOS_FLAG_6_REGISTER                = 0x6E,
        CONFIGURATION_4_REGISTER                        = 0x6F,

        // These are not real HW registers, they are synthetic for vm.  Real HW treats them as reserved.
        CUSTOM_VS_1_REGISTER                            = 0x70,     // remove?
        CUSTOM_VS_2_REGISTER                            = 0x71,     // remove?
        CUSTOM_VS_BIOS_LOGO_REGISTER                    = 0x72,
        CUSTOM_VS_GENERAL_EXTENSION_REGISTER            = 0x73,     // bios clear screen, etc.
    }
}

impl From<CrtControlReg> for u8 {
    fn from(reg: CrtControlReg) -> Self {
        reg.0
    }
}

impl From<u8> for CrtControlReg {
    fn from(reg: u8) -> Self {
        Self(reg)
    }
}

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum VgaGraphicsReg: u8 {
        SET_RESET_DATA_REGISTER             = 0,
        ENABLE_SET_RESET_DATA_REGISTER      = 1,
        COLOR_COMPARE_REGISTER              = 2,
        RASTER_OP_ROTATE_COUNT_REGISTER     = 3,
        READ_PLANE_SELECT_REGISTER          = 4,
        MODE_REGISTER                       = 5,
        MEMORY_MAP_MODE_CONTROL_REGISTER    = 6,
        COLOR_DONT_CARE_REGISTER            = 7,
        BIT_MASK_REGISTER                   = 8,
    }
}

impl From<VgaGraphicsReg> for u8 {
    fn from(reg: VgaGraphicsReg) -> Self {
        reg.0
    }
}

impl From<u8> for VgaGraphicsReg {
    fn from(reg: u8) -> Self {
        Self(reg)
    }
}

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum VgaAttribReg: u8 {
        PALETTE_0_REGISTER                 = 0x00,
        PALETTE_F_REGISTER                 = 0x0F,
        MODE_CONTROL_REGISTER              = 0x10,
        OVERSCAN_COLOR_REGISTER            = 0x11, // completely ignored by us
        COLOR_PLANE_ENABLE_REGISTER        = 0x12,
        HORIZONTAL_PIXEL_PANNING_REGISTER  = 0x13,
        PIXEL_PADDING_REGISTER             = 0x14,
        VGA_EXTENSION_REGISTER_16          = 0x16, // used in detection of Tseng Labs ET4000
    }
}

impl From<VgaAttribReg> for u8 {
    fn from(reg: VgaAttribReg) -> Self {
        reg.0
    }
}

impl From<u8> for VgaAttribReg {
    fn from(reg: u8) -> Self {
        Self(reg)
    }
}

pub const ATTRIBUTE_CONTROLLER_MODE_CONTROL_IPS_MASK: u8 = 0x80;
pub const ATTRIBUTE_CONTROLLER_MODE_CONTROL_AG_MASK: u8 = 0x01;

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum VgaSequencerReg: u8 {
        RESET_REGISTER                                    = 0x00,
        CLOCKING_MODE_REGISTER                            = 0x01,
        PLANE_WRITE_MASK_REGISTER                         = 0x02,
        CHARACTER_FONT_SELECT_REGISTER                    = 0x03,
        MEMORY_MODE_CONTROL_REGISTER                      = 0x04,

        UNLOCK_S3_EXTENDED_SEQUENCER_REGISTERS_REGISTER   = 0x08,

        // These are new for Trio64 over 928

        UNSUPPORTED_09_REGISTER                           = 0x09, // Extended Sequencer 9 Register
        UNSUPPORTED_0A_REGISTER                           = 0x0A, // Extended Sequencer A Register
        UNSUPPORTED_0B_REGISTER                           = 0x0B, // Extended Sequencer B Register

        UNSUPPORTED_0D_REGISTER                           = 0x0D, // Extended Sequencer D Register

        UNSUPPORTED_10_REGISTER                           = 0x10, // MCLK Value Low Register
        UNSUPPORTED_11_REGISTER                           = 0x11, // MCLK Value High Register
        UNSUPPORTED_12_REGISTER                           = 0x12, // DCLK Value Low Register
        UNSUPPORTED_13_REGISTER                           = 0x13, // DCLK Value High Register
        UNSUPPORTED_14_REGISTER                           = 0x14, // CLKSYN Control 1 Register
        UNSUPPORTED_15_REGISTER                           = 0x15, // CLKSYN Control 2 Register
        UNSUPPORTED_16_REGISTER                           = 0x16, // CLKSYN Test High Register
        UNSUPPORTED_17_REGISTER                           = 0x17, // CLKSYN Test Low Register
        UNSUPPORTED_18_REGISTER                           = 0x18, // RAMDAC/CLKSYN Control Register

        UNSUPPORTED_1C_REGISTER                           = 0x1C, // Extended Sequencer 1C Register
    }
}

impl From<VgaSequencerReg> for u8 {
    fn from(reg: VgaSequencerReg) -> Self {
        reg.0
    }
}

impl From<u8> for VgaSequencerReg {
    fn from(reg: u8) -> Self {
        Self(reg)
    }
}

pub const SEQ_MODE_ODD_EVEN_MASK: u8 = 0x04;
pub const GC_MODE_ODD_EVEN_MASK: u8 = 0x10;

pub const CRT_UNDERLINE_MODE_DWMASK: u8 = 0x40;
pub const CRT_UNDERLINE_MODE_CB4_MASK: u8 = 0x20;
pub const CRT_MODE_CONTROL_WBMASK: u8 = 0x40;
pub const CRT_MODE_CONTROL_AWMASK: u8 = 0x20;
pub const CRT_MAX_SCAN_LINE_MASK: u8 = 0x1F;

pub const SEQ_MEM_MODE_CHAIN4_MASK: u8 = 0x08;

pub const DEFAULT_MISC_OUTPUT_REG_VALUE: u8 = 0x40;
pub const MISC_OUTPUT_EMULATE_CGAMASK: u8 = 0x01;

pub const CRT_DISABLE_RETRACE_ITRP_MASK: u8 = 0x20;

pub const MAX_VGA_PIXELS_PER_ROW: u16 = 800;

pub const VGA_HIRES_CHARACTER_WIDTH: u8 = 8;
pub const VGA_LORES_CHARACTER_WIDTH: u8 = 16;
pub const DEFAULT_VGA_CHARACTER_HEIGHT: u8 = 16;

pub const TOTAL_VGA_HIRES_TEXT_COLUMNS: u8 = 80;
pub const TOTAL_VGA_LORES_TEXT_COLUMNS: u8 = 40;
pub const TOTAL_VGA_NORMAL_TEXT_ROWS: u8 = 25;
pub const TOTAL_VGA_MAX_TEXT_ROWS: u8 = 60;

pub const CGA_CHARACTER_BLINKING_MASK: u16 = 0x80;
pub const CGA_CHARACTER_BACKGROUND_COLOR_MASK: u16 = 0xF0;
pub const CGA_CHARACTER_FOREGROUND_COLOR_MASK: u16 = 0x0F;

pub const CURSOR_SCAN_LINE_MASK: u8 = 0x1F;
pub const CURSOR_ENABLED_FLAG: u8 = 0x20;

pub const CGA_BLINK_TIME_US: u32 = 250000;
pub const CURSOR_BLINK_TIME_US: u32 = 120000;

pub const BIOS_LOGO_VRAM_OFFSET: u32 = 0x100000;

pub const HORIZONTAL_RETRACE_INQUIRIES: u32 = 10;

pub const BIOS_CLEAR_SCREEN_CODE: u8 = 0x53;
pub const SYNTHVID_BLUE_SCREEN_CODE: u8 = 0xBD;

pub const VGA_FUNCTION_SELECT_NORMAL: u8 = 0;
pub const VGA_FUNCTION_SELECT_AND: u8 = 1;
pub const VGA_FUNCTION_SELECT_OR: u8 = 2;
pub const VGA_FUNCTION_SELECT_XOR: u8 = 3;

pub const VGA_WRITE_MODE_0: u8 = 0;
pub const VGA_WRITE_MODE_1: u8 = 1;
pub const VGA_WRITE_MODE_2: u8 = 2;
pub const VGA_WRITE_MODE_3: u8 = 3;

pub const VGA_READ_MODE_0: u8 = 0;
pub const VGA_READ_MODE_1: u8 = 1;
