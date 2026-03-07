// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod bluescreen;

use crate::render::GraphicsRenderState;
use crate::render::RenderControl;
use crate::render::RenderState;
use crate::render::TextRenderState;
use crate::spec;
use crate::spec::CrtControlReg;
use crate::spec::VGA_FUNCTION_SELECT_AND;
use crate::spec::VGA_FUNCTION_SELECT_NORMAL;
use crate::spec::VGA_FUNCTION_SELECT_OR;
use crate::spec::VgaAttribReg;
use crate::spec::VgaGraphicsReg;
use crate::spec::VgaPort;
use crate::spec::VgaSequencerReg;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use framebuffer::FramebufferLocalControl;
use guestmem::GuestMemory;
use guestmem::MapRom;
use guestmem::UnmapRom;
use inspect::Inspect;
use memory_range::MemoryRange;
use pci_core::spec::cfg_space::HeaderType00;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::ops::Index;
use std::ops::IndexMut;
use vmcore::vmtime::VmTimeAccess;
use vmcore::vmtime::VmTimeSource;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Inspect)]
pub struct Emulator {
    state: VgaAddressingVars,
    text: TextModeState,
    #[inspect(skip)]
    vmtime: VmTimeAccess,
    #[inspect(skip)]
    control: FramebufferLocalControl,
    #[inspect(skip)]
    rom: Option<Box<dyn MapRom>>,
    #[inspect(skip)]
    mapped_rom: Option<Box<dyn UnmapRom>>,
    #[inspect(skip)]
    vram: GuestMemory,
    vram_size: u32,
    is_mode_change_pending: bool,
    is_full_refresh_pending: bool,
    is_delayed_redraw_timer_set: bool,
    is_legacy_writes_timer_set: bool,
    suppress_initial_activation: bool,
    palette_change_count: u32,
    pixel_values_palette_count: u32,
    #[inspect(skip)]
    mapping_table: [u32; 256],
    #[inspect(skip)]
    render_control: RenderControl,
}

#[derive(Inspect, Clone, PartialEq, Eq)]
pub struct TextModeState {
    // Everything in this structure is derived from registers or is
    // re-set to continue blinking something on restore.  Nothing
    // in this structure should need to be saved and restored on
    // device save/restore.
    pub current_text_columns: u16,

    pub lo_res_text_mode: bool,
    pub mono_text_mode: bool,
    pub character_set_512: bool,
    pub char_set_1: u8,
    pub char_set_2: u8,

    pub text_char_width: u8,
    pub text_char_height: u8,
    pub text_rows: u8,

    // Blinking text variables
    pub blinking_enabled: bool,
    pub blinking_state: bool,

    // Text cursor variables
    pub draw_text_cursor: bool,
    pub cursor_first_scanline: u16,
    pub cursor_last_scanline: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_blink_state: bool,
}

impl TextModeState {
    fn new() -> Self {
        Self {
            current_text_columns: spec::TOTAL_VGA_HIRES_TEXT_COLUMNS.into(),
            lo_res_text_mode: false,
            mono_text_mode: false,
            character_set_512: false,
            char_set_1: 0,
            char_set_2: 0,
            text_char_width: spec::VGA_HIRES_CHARACTER_WIDTH,
            text_char_height: spec::DEFAULT_VGA_CHARACTER_HEIGHT,
            text_rows: spec::TOTAL_VGA_NORMAL_TEXT_ROWS,
            blinking_enabled: false,
            blinking_state: false,
            draw_text_cursor: false,
            cursor_first_scanline: 0,
            cursor_last_scanline: 0,
            cursor_row: 0,
            cursor_col: 0,
            cursor_blink_state: true,
        }
    }
}

#[derive(Inspect)]
struct VgaAddressingVars {
    persistent_state: VideoS3DeviceState,

    // Everything from here down in the structure is derived from registers on
    // restore, and nothing from here down should need to be saved or restored
    // on device save/restore.
    video_start_bus_range_offset: u64,
    video_end_bus_range_offset: u64,

    video_window_offset: u32,

    chain_4_mode: bool,
    odd_even_mode: bool,
    dbl_word_mode: bool,

    // Write-related values
    write_mode: u8,
    plane_write_mask: u8,
    data_rotate_value: u8,
    set_reset_value: u8,
    set_reset_mask: u8,
    function_select: u8,

    plane_3_in_use: bool,
    plane_write_mask32: u32, // derived from fPlaneWriteMask
    set_reset_mask32: u32,   // derived from fSetResetMask

    set_reset_value32: u32,      // derived from fSetResetValue
    set_reset_mask_value32: u32, // derived from fSetResetValue32 & fSetResetMask32

    pixel_mask32: u32,

    // Read-related values
    read_mode: u8,
    plane_read_num: u8,
    color_compare_value: u8, // only used to compute fColorCompareMask32  - maybe just use a local variable instead.
    color_dont_care: u8, // only used to compute fColorDontCareMask32 - maybe just use a local variable instead.

    color_compare_mask32: u32,   // derived from fColorCompareValue
    color_dont_care_mask32: u32, // derived from fColorDontCare
}

impl VgaAddressingVars {
    fn new(vram_size: usize) -> Self {
        Self {
            persistent_state: VideoS3DeviceState::new(vram_size),
            video_start_bus_range_offset: 0xa0000,
            video_end_bus_range_offset: 0xc0000,
            video_window_offset: 0,
            chain_4_mode: false,
            odd_even_mode: true,
            dbl_word_mode: false,
            write_mode: 0,
            plane_write_mask: 0xf,
            data_rotate_value: 0,
            set_reset_value: 0,
            set_reset_mask: 0,
            function_select: 0,
            plane_3_in_use: false,
            plane_write_mask32: 0,
            set_reset_mask32: 0,
            set_reset_value32: 0,
            set_reset_mask_value32: 0,
            pixel_mask32: 0,
            read_mode: 0,
            plane_read_num: 0,
            color_compare_value: 0,
            color_dont_care: 0,
            color_compare_mask32: 0,
            color_dont_care_mask32: 0,
        }
    }
}

#[derive(Inspect)]
struct VideoS3DeviceState {
    crt_control_regs: RegisterSet<CrtControlReg, 256>,
    crt_control_regs_shadow: RegisterSet<CrtControlReg, 256>,
    vga_sequencer_regs: RegisterSet<VgaSequencerReg, 32>,
    vga_sequencer_regs_shadow: RegisterSet<VgaSequencerReg, 32>,
    pub vga_attrib_regs: RegisterSet<VgaAttribReg, 32>,
    vga_graphics_regs: RegisterSet<VgaGraphicsReg, 16>,
    vga_graphics_regs_shadow: RegisterSet<VgaGraphicsReg, 16>,

    crt_control_index_reg: CrtControlReg,
    vga_seq_index_reg: VgaSequencerReg,
    vga_seq_index_reg_shadow: u8,
    vga_attrib_reg_index: u8,
    vga_attrib_reg_flip_flop: bool,
    vga_graphics_reg_index: VgaGraphicsReg,
    vga_graphics_reg_index_shadow: u8,

    #[inspect(skip)]
    pel_colors: [PelColor; 256],
    pel_reg_write_index: u16,
    pel_reg_read_index: u16,

    pel_mask_register: u8,

    text_mode: bool,
    video_enabled: bool,
    misc_output_reg: u8,

    crt_regs_locked: bool,

    horizontal_retrace: bool,
    horizontal_retrace_count: u8,

    adj_pcvideo_height: u16,
    pcvideo_height: u16,
    pcvideo_width: u16,
    line_offset_pixels: u16,
    line_compare_value: u16,
    cur_page_start_offset: u32,

    video_pci_status: u32,
    interrupt_line_info: u32,

    bits_per_pixel: u8,
    enhanced_dac_mode: bool,

    latched_read_value: u32,

    s3: S3ControllerState,

    #[inspect(hex)]
    expansion_rom_base: u32,
}

impl VideoS3DeviceState {
    fn new(vram_size: usize) -> Self {
        let crt_control_regs = RegisterSet::new([
            (
                CrtControlReg::S3_CHIP_REVISION_NUMBER_REGISTER,
                spec::S3_TRIO_CHIPSET_REV_NUMBER,
            ),
            (
                CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_2_REGISTER,
                0xA,
            ),
            (
                CrtControlReg::S3_DEVICE_ID_HI_REGISTER,
                (spec::PCI_DEVICE_ID >> 8) as u8,
            ),
            (
                CrtControlReg::S3_DEVICE_ID_LO_REGISTER,
                spec::PCI_DEVICE_ID as u8,
            ),
            (
                CrtControlReg::S3_DEVICE_REVISION_REGISTER,
                spec::PCI_REVISION,
            ),
            (
                CrtControlReg::S3_CONFIGURATION_1_REGISTER,
                if vram_size == 0x100000 {
                    0xC2
                } else if vram_size == 0x200000 {
                    0x82
                } else {
                    0x02
                },
            ),
            (CrtControlReg::HORIZONTAL_TOTAL_REGISTER, 0),
            (CrtControlReg::HORIZONTAL_DISPLAY_END_REGISTER, 0),
            (CrtControlReg::START_HORIZONTAL_BLANK_REGISTER, 0),
            (CrtControlReg::END_HORIZONTAL_BLANK_REGISTER, 0),
            (CrtControlReg::START_HORIZONTAL_RETRACE_REGISTER, 0),
            (CrtControlReg::END_HORIZONTAL_RETRACE_REGISTER, 0),
            (CrtControlReg::VERTICAL_TOTAL_REGISTER, 0),
            (CrtControlReg::OVERFLOW_REGISTER, 0),
            (CrtControlReg::PRESET_ROW_SCAN_REGISTER, 0),
            (CrtControlReg::MAX_SCANLINE_REGISTER, 0),
            (CrtControlReg::CURSOR_START_REGISTER, 0),
            (CrtControlReg::CURSOR_END_REGISTER, 0),
            (CrtControlReg::START_ADDRESS_HI_REGISTER, 0),
            (CrtControlReg::START_ADDRESS_LO_REGISTER, 0),
            (CrtControlReg::CURSOR_LOCATION_HI_REGISTER, 0),
            (CrtControlReg::CURSOR_LOCATION_LO_REGISTER, 0),
            (CrtControlReg::VERTICAL_RETRACE_HI_REGISTER, 0),
            (CrtControlReg::VERTICAL_RETRACE_LO_REGISTER, 0),
            (CrtControlReg::VERTICAL_DISPLAY_END_REGISTER, 0),
            (CrtControlReg::OFFSET_REGISTER, 0),
            (CrtControlReg::UNDERLINE_LOCATION_REGISTER, 0),
            (CrtControlReg::START_VERTICAL_BLANK_REGISTER, 0),
            (CrtControlReg::END_VERTICAL_BLANK_REGISTER, 0),
            (CrtControlReg::MODE_CONTROL_REGISTER, 0),
            (CrtControlReg::LINE_COMPARE_REGISTER, 0),
            (CrtControlReg::UNSUPPORTED_22_REGISTER, 0),
            (CrtControlReg::UNSUPPORTED_24_REGISTER, 0),
            (CrtControlReg::S3_MEMORY_CONFIGURATION_REGISTER, 0),
            (CrtControlReg::S3_BACKWARD_COMPATIBILITY_1_REGISTER, 0),
            (CrtControlReg::S3_BACKWARD_COMPATIBILITY_2_REGISTER, 0),
            (CrtControlReg::S3_BACKWARD_COMPATIBILITY_3_REGISTER, 0),
            (CrtControlReg::S3_REGISTER_LOCK_REGISTER, 0),
            (CrtControlReg::S3_CONFIGURATION_2_REGISTER, 0),
            (CrtControlReg::S3_UNLOCK_VGA_REGISTERS_1_REGISTER, 0),
            (CrtControlReg::S3_UNLOCK_VGA_REGISTERS_2_REGISTER, 0),
            (CrtControlReg::S3_MISC_1_REGISTER, 0),
            (CrtControlReg::S3_DATA_TRANSFER_REGISTER, 0),
            (CrtControlReg::S3_INTERLACE_START_REGISTER, 0),
            (CrtControlReg::S3_SYSTEM_CONFIGURATION_REGISTER, 0),
            (CrtControlReg::S3_BIOS_FLAG_REGISTER, 0),
            (CrtControlReg::S3_MODE_CONTROL_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MODE_REGISTER, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_1, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_2, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_3, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_4, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_5, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_6, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_7, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_8, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_9, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_10, 0),
            (CrtControlReg::S3_HW_CURSOR_DEAD_11, 0),
            (CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_1_REGISTER, 0),
            (CrtControlReg::S3_MMIO_DEAD_1, 0),
            (CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_2_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_DAC_CONTROL_REGISTER, 0),
            (CrtControlReg::S3_EXTERNAL_SYNC_CONTROL_1_REGISTER, 0),
            (CrtControlReg::S3_EXTERNAL_SYNC_CONTROL_2_REGISTER, 0),
            (CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_CONTROL_REGISTER, 0),
            (
                CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_1_REGISTER,
                0,
            ),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_2_REGISTER, 0),
            (CrtControlReg::S3_GENERAL_OUTPUT_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_HORIZONTAL_OVERFLOW_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BUS_GRANT_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_3_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_4_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_5_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MISC_CONTROL_0_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MISC_CONTROL_1_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_MISC_CONTROL_2_REGISTER, 0),
            (CrtControlReg::S3_CONFIGURATION_3_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_3_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_4_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_5_REGISTER, 0),
            (CrtControlReg::S3_EXTENDED_BIOS_FLAG_6_REGISTER, 0),
            (CrtControlReg::CONFIGURATION_4_REGISTER, 0),
            (CrtControlReg::CUSTOM_VS_1_REGISTER, 0),
            (CrtControlReg::CUSTOM_VS_2_REGISTER, 0),
            (CrtControlReg::CUSTOM_VS_BIOS_LOGO_REGISTER, 0),
            (CrtControlReg::CUSTOM_VS_GENERAL_EXTENSION_REGISTER, 0),
        ]);

        let vga_sequencer_regs = RegisterSet::new([
            (VgaSequencerReg::PLANE_WRITE_MASK_REGISTER, 0xf),
            (VgaSequencerReg::RESET_REGISTER, 0),
            (VgaSequencerReg::CLOCKING_MODE_REGISTER, 0),
            (VgaSequencerReg::CHARACTER_FONT_SELECT_REGISTER, 0),
            (VgaSequencerReg::MEMORY_MODE_CONTROL_REGISTER, 0),
            (
                VgaSequencerReg::UNLOCK_S3_EXTENDED_SEQUENCER_REGISTERS_REGISTER,
                0,
            ),
            (VgaSequencerReg::UNSUPPORTED_09_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_0A_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_0B_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_0D_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_10_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_11_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_12_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_13_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_14_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_15_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_16_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_17_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_18_REGISTER, 0),
            (VgaSequencerReg::UNSUPPORTED_1C_REGISTER, 0),
        ]);

        let vga_attrib_regs = RegisterSet::new((0..=0x14).map(|n| (VgaAttribReg(n), 0)));

        let vga_graphics_regs = RegisterSet::new([
            (VgaGraphicsReg::SET_RESET_DATA_REGISTER, 0),
            (VgaGraphicsReg::ENABLE_SET_RESET_DATA_REGISTER, 0),
            (VgaGraphicsReg::COLOR_COMPARE_REGISTER, 0),
            (VgaGraphicsReg::RASTER_OP_ROTATE_COUNT_REGISTER, 0),
            (VgaGraphicsReg::READ_PLANE_SELECT_REGISTER, 0),
            (VgaGraphicsReg::MODE_REGISTER, 0),
            (VgaGraphicsReg::MEMORY_MAP_MODE_CONTROL_REGISTER, 0),
            (VgaGraphicsReg::COLOR_DONT_CARE_REGISTER, 0),
            (VgaGraphicsReg::BIT_MASK_REGISTER, 0xff),
            (VgaGraphicsReg(9), 9),
            (VgaGraphicsReg(10), 10),
            (VgaGraphicsReg(11), 11),
            (VgaGraphicsReg(12), 12),
            (VgaGraphicsReg(13), 13),
            (VgaGraphicsReg(14), 14),
            (VgaGraphicsReg(15), 15),
        ]);

        Self {
            crt_control_regs_shadow: crt_control_regs.clone(),
            crt_control_regs,
            vga_sequencer_regs_shadow: vga_sequencer_regs.clone(),
            vga_sequencer_regs,
            vga_attrib_regs,
            vga_graphics_regs_shadow: vga_graphics_regs.clone(),
            vga_graphics_regs,
            crt_control_index_reg: CrtControlReg(0),
            vga_seq_index_reg: VgaSequencerReg(0),
            vga_seq_index_reg_shadow: 0,
            vga_attrib_reg_index: 0,
            vga_attrib_reg_flip_flop: true,
            vga_graphics_reg_index: VgaGraphicsReg(0),
            vga_graphics_reg_index_shadow: 0,
            pel_colors: FromZeros::new_zeroed(),
            pel_reg_write_index: 0,
            pel_reg_read_index: 0,
            pel_mask_register: 0xFF,
            text_mode: true,
            video_enabled: true,
            misc_output_reg: spec::DEFAULT_MISC_OUTPUT_REG_VALUE,
            crt_regs_locked: false,
            horizontal_retrace: false,
            horizontal_retrace_count: 0,
            adj_pcvideo_height: 400,
            pcvideo_height: 400,
            pcvideo_width: spec::MAX_VGA_PIXELS_PER_ROW,
            line_offset_pixels: 0,
            line_compare_value: 0,
            cur_page_start_offset: 0,
            video_pci_status: 0,
            interrupt_line_info: 0xFF,
            bits_per_pixel: 4,
            enhanced_dac_mode: false,
            latched_read_value: 0,
            s3: S3ControllerState::new(),
            expansion_rom_base: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct PelColor {
    red: u8,
    green: u8,
    blue: u8,
}

#[derive(Inspect)]
struct S3ControllerState {
    enhanced_mode: bool,
    linear_mapping: bool,
    #[inspect(hex)]
    addr_window_size: u32,
    #[inspect(hex)]
    addr_window_base: u32,
    addr_window_offset: u32,
    #[inspect(hex)]
    linear_addr_window: u32,
    bits_per_pixel: u16,
    adv_function_control_reg: u16,
    screen_pixel_width: u16,
}

impl S3ControllerState {
    fn new() -> Self {
        Self {
            enhanced_mode: false,
            linear_mapping: false,
            addr_window_size: 0,
            addr_window_base: 0,
            addr_window_offset: 0,
            linear_addr_window: 0xa0000,
            bits_per_pixel: 0,
            adv_function_control_reg: 0,
            screen_pixel_width: 1024,
        }
    }
}

#[derive(Clone)]
pub struct RegisterSet<T, const N: usize>([u8; N], PhantomData<fn(T)>);

impl<T: From<u8> + Debug, const N: usize> Inspect for RegisterSet<T, N> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (i, v) in self.0.iter().enumerate() {
            resp.hex(&format!("{:#x?}", T::from(i as u8)), v);
        }
    }
}

impl<T: Into<u8>, const N: usize> RegisterSet<T, N> {
    fn new(values: impl IntoIterator<Item = (T, u8)>) -> Self {
        // Default unknown registers to 0x80.
        let mut this = Self([0x80; N], PhantomData);
        for (x, y) in values {
            this[x] = y;
        }
        this
    }
}

impl<T: Into<u8>, const N: usize> Index<T> for RegisterSet<T, N> {
    type Output = u8;

    fn index(&self, index: T) -> &Self::Output {
        &self.0[index.into() as usize]
    }
}

impl<T: Into<u8>, const N: usize> IndexMut<T> for RegisterSet<T, N> {
    fn index_mut(&mut self, index: T) -> &mut Self::Output {
        &mut self.0[index.into() as usize]
    }
}

const SPLASH_SCREEN_BYTES_PER_PIXEL: u8 = 2;
const _SPLASH_SCREEN_WIDTH: u16 = 640;
const _SPLASH_SCREEN_HEIGHT: u16 = 400;

/// This routine expands a four-bit mask into a 32-bit reversed
/// mask. Each bit in the original value represents a byte in the
/// final mask. The original bits are in reverse order (i.e.
/// the lsb of the original represents the most-significant byte
/// of the final mask.
fn expand_mask(template: u8) -> u32 {
    let mut mask = 0;
    if template & 0x1 != 0 {
        mask |= make_mask_for_byte(0);
    }
    if template & 0x2 != 0 {
        mask |= make_mask_for_byte(1);
    }
    if template & 0x4 != 0 {
        mask |= make_mask_for_byte(2);
    }
    if template & 0x8 != 0 {
        mask |= make_mask_for_byte(3);
    }

    mask
}

/// Make 1 Byte mask at ByteNumber position in a 32 bit number.
fn make_mask_for_byte(byte_number: u32) -> u32 {
    0xFF << (byte_number * 8)
}

impl Emulator {
    pub fn reset(&mut self) {
        self.state = VgaAddressingVars::new(self.vram_size as usize);
        self.control.unmap();
        self.mapped_rom = None;
        self.update_render_state();
    }

    pub fn new(
        control: FramebufferLocalControl,
        vram: GuestMemory,
        vmtime: &VmTimeSource,
        rom: Option<Box<dyn MapRom>>,
        render_control: RenderControl,
    ) -> Self {
        let vram_size = control.len();
        Self {
            state: VgaAddressingVars::new(vram_size),
            text: TextModeState::new(),
            vmtime: vmtime.access("vga"),
            control,
            vram,
            rom,
            mapped_rom: None,
            vram_size: vram_size as u32,
            is_mode_change_pending: false,
            is_full_refresh_pending: false,
            is_delayed_redraw_timer_set: false,
            is_legacy_writes_timer_set: false,
            suppress_initial_activation: false,
            palette_change_count: 1,
            pixel_values_palette_count: 0,
            mapping_table: [0; 256],
            render_control,
        }
    }

    pub fn notify_pci_config_access_write(
        &mut self,
        in_reg_address: u16,
        io_data: u32,
    ) -> IoResult {
        tracing::trace!(
            reg = ?HeaderType00(in_reg_address),
            io_data,
            "pci config write"
        );

        match HeaderType00(in_reg_address) {
            HeaderType00::STATUS_COMMAND => {
                // Write the command register portion of the status/command register
                // The WHQL video tests require that these bits be read-only: 0x06FFF800
                self.state.persistent_state.video_pci_status = io_data & 0x07FF;
            }

            HeaderType00::BIST_HEADER => return IoResult::Err(IoError::InvalidRegister),

            HeaderType00::BAR0 => {
                // Clear low-order four bits to indicate the base
                // register is for memory that can be anywhere in
                // the 32-bit address space. The other bits are zeroed
                // to guarantee that the plug-n-play software
                // allocates an address range that is correctly sized and
                // aligned.
                self.s3_set_linear_address_base(io_data as u64 & 0xFC000000); // S3 device
            }

            HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => return IoResult::Err(IoError::InvalidRegister),

            HeaderType00::LATENCY_INTERRUPT => {
                self.state.persistent_state.interrupt_line_info = io_data;
            }

            HeaderType00::EXPANSION_ROM_BASE => {
                if let Some(rom) = &self.rom {
                    let reg = io_data & 0xFFFF0001;
                    if reg != self.state.persistent_state.expansion_rom_base {
                        self.state.persistent_state.expansion_rom_base = reg;
                        self.mapped_rom = None;
                        if reg & 1 != 0 {
                            match rom.map_rom((reg & !1).into(), 0, rom.len()) {
                                Ok(mapping) => self.mapped_rom = Some(mapping),
                                Err(err) => {
                                    tracing::error!(
                                        error = &err as &dyn std::error::Error,
                                        "failed to map expansion ROM"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            reg => {
                tracing::warn!(?reg, data = io_data, "unhandled vga config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    pub fn notify_pci_config_access_read(
        &self,
        in_reg_address: u16,
        io_data: &mut u32,
    ) -> IoResult {
        tracing::trace!(
            reg = ?HeaderType00(in_reg_address),
            "pci config read"
        );
        *io_data = match HeaderType00(in_reg_address) {
            HeaderType00::DEVICE_VENDOR => {
                // Use constant Vendor ID and configured Device ID
                spec::PCI_VENDOR_ID as u32 | ((spec::PCI_DEVICE_ID as u32) << 16)
            }

            HeaderType00::STATUS_COMMAND => self.state.persistent_state.video_pci_status,

            HeaderType00::CLASS_REVISION => {
                // Use constant class code and configured revision

                ((spec::PCI_VIDEO_CLASS_CODE as u32) << 24) | (spec::PCI_REVISION as u32)
            }

            HeaderType00::BIST_HEADER => {
                // Specify default value (header type zero, single-function card)
                0
            }

            HeaderType00::SUBSYSTEM_ID => spec::PCI_SUBSYSTEM.into(),

            HeaderType00::BAR0 => self.state.persistent_state.s3.linear_addr_window & 0xFC000000,

            HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                // These registers are not implemented
                return IoResult::Err(IoError::InvalidRegister);
            }

            HeaderType00::LATENCY_INTERRUPT => {
                // The device is hard-wired to PCI interrupt lane A.
                let pci_irq_lane_a = 1;
                (pci_irq_lane_a << 8) | (self.state.persistent_state.interrupt_line_info & 0xFF)
            }

            HeaderType00::EXPANSION_ROM_BASE => {
                // 64KB ROM
                self.state.persistent_state.expansion_rom_base & 0xFFFF0001
            }

            reg => {
                tracing::warn!(?reg, "unhandled vga config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        tracing::trace!(
            reg = ?HeaderType00(in_reg_address),
            io_data,
            "pci config read finished"
        );
        IoResult::Ok
    }

    pub fn notify_mmio_read(&mut self, address: u64, data: &mut [u8]) {
        if (address >= self.state.video_start_bus_range_offset)
            && (address < self.state.video_end_bus_range_offset)
        {
            let start = (address - self.state.video_start_bus_range_offset) as u32;
            // It's important to read from the MSB to the LSB. If we
            // do it in the other order, we don't get the correct
            // latching behavior in the VGA controller.
            match self.state.read_mode {
                spec::VGA_READ_MODE_0 => {
                    for (offset, byte) in data.iter_mut().enumerate().rev() {
                        *byte = self.handle_vgaread0(start + offset as u32);
                    }
                }
                spec::VGA_READ_MODE_1 => {
                    for (offset, byte) in data.iter_mut().enumerate().rev() {
                        *byte = self.handle_vgaread1(start + offset as u32);
                    }
                }
                _ => unreachable!(),
            }
        } else {
            data.fill(!0);
        }
    }

    pub fn notify_mmio_write(&mut self, address: u64, data: &[u8]) {
        if (address >= self.state.video_start_bus_range_offset)
            && (address < self.state.video_end_bus_range_offset)
        {
            let start = (address - self.state.video_start_bus_range_offset) as u32;
            // It's important to write from the MSB to the LSB. If we
            // do it in the other order, we don't get the correct
            // latching behavior in the VGA controller.
            match self.state.write_mode {
                spec::VGA_WRITE_MODE_0 => {
                    for (offset, &byte) in data.iter().enumerate().rev() {
                        self.handle_vgawrite0(start + offset as u32, byte);
                    }
                }

                spec::VGA_WRITE_MODE_1 => {
                    for (offset, _) in data.iter().enumerate().rev() {
                        self.handle_vgawrite1(start + offset as u32);
                    }
                }

                spec::VGA_WRITE_MODE_2 => {
                    for (offset, &byte) in data.iter().enumerate().rev() {
                        self.handle_vgawrite2(start + offset as u32, byte);
                    }
                }

                spec::VGA_WRITE_MODE_3 => {
                    for (offset, &byte) in data.iter().enumerate().rev() {
                        self.handle_vgawrite3(start + offset as u32, byte);
                    }
                }

                _ => {}
            }
        }
        self.update_render_state();
    }

    /// This routine adjusts the address for odd/even or chain4 modes.
    /// It also adjusts the plane mask in the process.  This routine makes
    /// the assumption that the caller intends to access 4-bytes worth of
    /// vram using the returned address, and range-checks accordingly.
    fn adjust_vga_address(
        &self,
        io_vga_addr: &mut u32,
        io_plane_mask: &mut u32,
        io_plane_to_read: &mut u32,
    ) -> bool {
        let mut vga_addr = *io_vga_addr;
        let mut plane_mask = *io_plane_mask;
        let mut read_plane = *io_plane_to_read;
        let mut within_vram = true;

        vga_addr += self.state.video_window_offset;

        if !self.state.persistent_state.s3.enhanced_mode {
            // Are we in word mode (for CGA/MDA compatibility)?
            if self.state.chain_4_mode {
                // The two lsb's of the address determine the plane to be accessed.
                read_plane = vga_addr & 0x3;
                plane_mask = make_mask_for_byte(read_plane);

                // Now clear the two lsb's of the address
                vga_addr &= !0x3;
            } else if self.state.odd_even_mode {
                // The lsb of the address determines whether odd
                // or even planes are accessible.
                if (vga_addr & 1) == 0 {
                    plane_mask &= make_mask_for_byte(0) | make_mask_for_byte(2);
                } else {
                    plane_mask &= make_mask_for_byte(1) | make_mask_for_byte(3);
                }

                read_plane = vga_addr & 1;

                // Now clear the lsb of the address
                vga_addr &= !0x1;
            }

            vga_addr <<= 2;
        }

        // Are we accessing memory outside of VRAM?
        // Again, assume caller will access 4-bytes worth.
        let vram_size = self.vram_size;

        if vga_addr > (vram_size - 4) {
            within_vram = false;
        }

        *io_vga_addr = vga_addr;
        *io_plane_mask = plane_mask;
        *io_plane_to_read = read_plane;

        within_vram
    }

    fn read_vram<T: IntoBytes + FromBytes + Immutable + KnownLayout>(&self, address: u32) -> T {
        self.vram
            .read_plain(address.into())
            .expect("framebuffer is mapped")
    }

    fn write_vram<T: IntoBytes + Immutable + KnownLayout>(&self, address: u32, value: T) {
        self.vram
            .write_plain(address.into(), &value)
            .expect("framebuffer is mapped")
    }

    fn handle_vgawrite0(&mut self, vga_addr: u32, write_data: u8) {
        let mut plane_to_read = 0;

        let mut vga_addr = vga_addr;
        let mut plane_mask = self.state.plane_write_mask32;
        let latched_value = self.state.persistent_state.latched_read_value;
        let pixel_mask = self.state.pixel_mask32;
        let function = self.state.function_select;

        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_read) {
            // OK, now we have the address and the plane mask computed,
            // we can go ahead and do the actual store.

            let mut new_value = (write_data as u32)
                | ((write_data as u32) << 8)
                | ((write_data as u32) << 16)
                | ((write_data as u32) << 24);

            // Perform data rotate if necessary
            if self.state.data_rotate_value != 0 {
                new_value = new_value.rotate_left(self.state.data_rotate_value.into());
            }

            // Adjust the input data using the set/reset registers
            new_value &= !self.state.set_reset_mask32;
            new_value |= self.state.set_reset_mask_value32;

            // Now perform the necessary ALU function
            if function != VGA_FUNCTION_SELECT_NORMAL {
                if function == VGA_FUNCTION_SELECT_AND {
                    new_value &= latched_value;
                } else if function == VGA_FUNCTION_SELECT_OR {
                    new_value |= latched_value;
                } else {
                    //  VGA_FUNCTION_SELECT_XOR
                    new_value ^= latched_value;
                }
            }

            let old_value: u32 = self.read_vram(vga_addr);
            new_value = (new_value & pixel_mask) | (latched_value & !pixel_mask);
            new_value = (new_value & plane_mask) | (old_value & !plane_mask);
            self.write_vram(vga_addr, new_value);
        }
    }

    fn handle_vgawrite1(&mut self, vga_addr: u32) {
        let mut vga_addr = vga_addr;
        let mut plane_to_read = 0;
        let mut plane_mask = self.state.plane_write_mask32;

        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_read) {
            let old_value: u32 = self.read_vram(vga_addr);
            let mut new_value = self.state.persistent_state.latched_read_value & plane_mask;
            new_value |= old_value & !plane_mask;
            self.write_vram(vga_addr, new_value);
        }
    }

    fn handle_vgawrite2(&mut self, vga_addr: u32, write_data: u8) {
        let mut vga_addr = vga_addr;
        let mut plane_mask = self.state.plane_write_mask32;
        let latched_value = self.state.persistent_state.latched_read_value;
        let pixel_mask = self.state.pixel_mask32;
        let mut plane_to_read = 0;

        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_read) {
            // OK, now we have the address and the plane mask computed,
            // we can go ahead and do the actual store.

            let mut new_value = expand_mask(write_data);

            // Now perform the necessary ALU function
            if self.state.function_select != VGA_FUNCTION_SELECT_NORMAL {
                if self.state.function_select == VGA_FUNCTION_SELECT_AND {
                    new_value &= latched_value;
                } else if self.state.function_select == VGA_FUNCTION_SELECT_OR {
                    new_value |= latched_value;
                } else {
                    // VGA_FUNCTION_SELECT_XOR
                    new_value ^= latched_value;
                }
            }

            let old_value: u32 = self.read_vram(vga_addr);

            new_value = (new_value & pixel_mask) | (latched_value & !pixel_mask);
            new_value = (new_value & plane_mask) | (old_value & !plane_mask);
            self.write_vram(vga_addr, new_value);
        }
    }

    fn handle_vgawrite3(&mut self, vga_addr: u32, write_data: u8) {
        let mut vga_addr = vga_addr;
        let mut plane_mask = self.state.plane_write_mask32;
        let latched_value = self.state.persistent_state.latched_read_value;
        let mut pixel_mask = self.state.pixel_mask32;
        let function = self.state.function_select;
        let mut plane_to_read = 0;

        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_read) {
            // OK, now we have the address and the plane mask computed,
            // we can go ahead and do the actual store.

            let v = write_data as u32;
            let mut new_value = v | (v << 8) | (v << 16) | (v << 24);

            // Perform data rotate if necessary
            if self.state.data_rotate_value != 0 {
                new_value = new_value.rotate_left(self.state.data_rotate_value.into());
            }

            pixel_mask &= new_value;
            new_value = self.state.set_reset_value32;

            // Now perform the necessary ALU function
            if function != VGA_FUNCTION_SELECT_NORMAL {
                if function == VGA_FUNCTION_SELECT_AND {
                    new_value &= latched_value;
                } else if function == VGA_FUNCTION_SELECT_OR {
                    new_value |= latched_value;
                } else {
                    // VGA_FUNCTION_SELECT_XOR
                    new_value ^= latched_value;
                }
            }

            let old_value: u32 = self.read_vram(vga_addr);

            new_value = (new_value & pixel_mask) | (latched_value & !pixel_mask);
            new_value = (new_value & plane_mask) | (old_value & !plane_mask);
            self.write_vram(vga_addr, new_value);
        }
    }

    fn handle_vgaread0(&mut self, vga_addr: u32) -> u8 {
        let mut plane_to_access = self.state.plane_read_num as u32;
        let mut vga_addr = vga_addr;
        let mut plane_mask = 0;

        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_access) {
            self.state.persistent_state.latched_read_value = self.read_vram(vga_addr);
            self.read_vram(vga_addr + plane_to_access)
        } else {
            0xFF
        }
    }

    fn handle_vgaread1(&mut self, vga_addr: u32) -> u8 {
        let mut vga_addr = vga_addr;
        let mut plane_mask = 0;
        let mut plane_to_access = 0;
        if self.adjust_vga_address(&mut vga_addr, &mut plane_mask, &mut plane_to_access) {
            let color_compare_mask = self.state.color_compare_mask32;
            let color_dont_care_mask = self.state.color_dont_care_mask32;

            let cur_value = self.read_vram(vga_addr);
            self.state.persistent_state.latched_read_value = cur_value;

            // Use XNOR function to determine which bits are the same
            let mut compare_value = !(color_compare_mask ^ cur_value);

            // OR in the color-don't-care mask for the specified planes
            compare_value |= color_dont_care_mask;

            // Finally, AND the four plane compare values together
            // to get a resulting 8-bit value.
            (compare_value & (compare_value >> 8) & (compare_value >> 16) & (compare_value >> 24))
                as u8
        } else {
            0xFF
        }
    }

    pub fn io_port_read(&mut self, address: u16, _access_size: u16) -> u32 {
        match address & 0xFFF0 {
            _ if address == spec::S3_ADV_FUNCTION_CONTROL_PORT => self
                .state
                .persistent_state
                .s3
                .adv_function_control_reg
                .into(),
            spec::VGA_HARDWARE_PORT_RANGE => match VgaPort(address) {
                VgaPort::INDEX_DATA_REG_ATTR_PORT | VgaPort::ATTRIBUTE_READ_PORT => {
                    self.attribute_io_read(VgaPort(address)).into()
                }
                VgaPort::SEQ_INDEX_REGISTER_PORT | VgaPort::SEQ_DATA_REGISTER_PORT => {
                    self.sequencer_io_read(VgaPort(address)).into()
                }
                VgaPort::GRAPHICS_INDEX_REG_PORT | VgaPort::GRAPHICS_DATA_REG_PORT => {
                    self.graphic_controller_io_read(VgaPort(address)).into()
                }
                address => self.read_vga_port(address).into(),
            },
            spec::MDA_HARDWARE_PORT_RANGE => self
                .read_cga(spec::CGA_HARDWARE_PORT_RANGE | (address & 0xF))
                .into(),
            spec::CGA_HARDWARE_PORT_RANGE => self.read_cga(address).into(),
            _ => !0,
        }
    }

    pub fn io_port_write(&mut self, address: u16, access_size: u16, data: u32) {
        match address & 0xFFF0 {
            _ if address == spec::S3_ADV_FUNCTION_CONTROL_PORT => {
                self.state.persistent_state.s3.adv_function_control_reg = data as u16;
                self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_MISC_CONTROL_1_REGISTER] &= !0x01;
                self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_MISC_CONTROL_1_REGISTER] |= data as u8 & 0x01;
                self.calculate_graphics_mode_variables();
            }
            spec::VGA_HARDWARE_PORT_RANGE => match VgaPort(address) {
                VgaPort::INDEX_DATA_REG_ATTR_PORT | VgaPort::ATTRIBUTE_READ_PORT => {
                    self.attribute_io_write(VgaPort(address), data)
                }
                VgaPort::SEQ_INDEX_REGISTER_PORT | VgaPort::SEQ_DATA_REGISTER_PORT => {
                    self.sequencer_io_write(VgaPort(address), access_size, data)
                }
                VgaPort::GRAPHICS_INDEX_REG_PORT | VgaPort::GRAPHICS_DATA_REG_PORT => {
                    self.graphic_controller_io_write(VgaPort(address), access_size, data)
                }
                address => self.write_vga_port(address, data as u8),
            },
            spec::MDA_HARDWARE_PORT_RANGE => self.write_cga(
                spec::CGA_HARDWARE_PORT_RANGE | (address & 0xF),
                &data.to_ne_bytes()[..access_size as usize],
            ),
            spec::CGA_HARDWARE_PORT_RANGE => {
                self.write_cga(address, &data.to_ne_bytes()[..access_size as usize])
            }
            _ => {}
        }
        self.update_render_state();
    }

    fn read_cga(&mut self, port: u16) -> u8 {
        match port {
            spec::CGA_INDEX_REGISTER_PORT => self.state.persistent_state.crt_control_index_reg.0,
            spec::CGA_DATA_REGISTER_PORT => match self.state.persistent_state.crt_control_index_reg
            {
                CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER
                | CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER => {
                    self.state.persistent_state.crt_control_regs_shadow
                        [self.state.persistent_state.crt_control_index_reg]
                }
                reg => self.state.persistent_state.crt_control_regs[reg],
            },
            spec::CGA_INPUT_STATUS_REG1_PORT => {
                self.state.persistent_state.vga_attrib_reg_flip_flop = true;
                // The following bits are always set in this register
                let mut val = 0xF4;

                if self.is_vertical_retrace_active() {
                    val |= 0x9;
                }

                // The horizontal retrace is similar. We will just toggle back
                // and forth here.
                if self.state.persistent_state.horizontal_retrace {
                    val |= 0x1;
                }

                self.state.persistent_state.horizontal_retrace_count += 1;
                if self.state.persistent_state.horizontal_retrace_count > 10 {
                    self.state.persistent_state.horizontal_retrace_count = 0;
                    self.state.persistent_state.horizontal_retrace =
                        !self.state.persistent_state.horizontal_retrace;
                }

                val
            }
            spec::CGA_MODE_CONTROL_REGISTER_PORT => 0x50,
            spec::CGA_UNKNOWN_PORT_3D3 => 0x80,
            spec::CGA_UNKNOWN_PORT_3D6 => 0x80,
            spec::CGA_UNKNOWN_PORT_3D7 => 0x80,
            spec::CGA_UNKNOWN_PORT_3DB => 0x80,
            spec::CGA_UNKNOWN_PORT_3DC => 0x80,
            spec::CGA_UNKNOWN_PORT_3DD => 0x80,
            spec::CGA_UNKNOWN_PORT_3DE => 0x80,
            spec::CGA_UNKNOWN_PORT_3DF => 0x80,
            _ => 0xff,
        }
    }

    fn write_cga(&mut self, port: u16, data: &[u8]) {
        for (&value, port) in data.iter().zip(port..) {
            match port {
                spec::CGA_INDEX_REGISTER_PORT => {
                    self.state.persistent_state.crt_control_index_reg = CrtControlReg(value)
                }
                spec::CGA_DATA_REGISTER_PORT => self.write_crt_control_register(
                    self.state.persistent_state.crt_control_index_reg,
                    value,
                ),
                _ => {}
            }
        }
    }

    fn attribute_io_read(&mut self, io_address: VgaPort) -> u8 {
        if io_address == VgaPort::INDEX_DATA_REG_ATTR_PORT {
            // This is technically documented as a write-only register, but
            // some software attempts to read it. We will let them so they are happy.
            self.state.persistent_state.vga_attrib_reg_index
        } else {
            let reg_address = self.state.persistent_state.vga_attrib_reg_index & 0x1F;
            self.state.persistent_state.vga_attrib_regs[VgaAttribReg(reg_address)]
        }
    }

    fn attribute_io_write(&mut self, io_address: VgaPort, write_data: u32) {
        if io_address == VgaPort::INDEX_DATA_REG_ATTR_PORT {
            if self.state.persistent_state.vga_attrib_reg_flip_flop {
                // Set the new attribute register address
                self.state.persistent_state.vga_attrib_reg_index = write_data as u8;
            } else {
                // Write the new attribute register value
                self.write_vga_attribute_reg(
                    VgaAttribReg(self.state.persistent_state.vga_attrib_reg_index & 0x1F),
                    write_data as u8,
                );
            }

            // The flip-flop changes state whenever this is written
            self.state.persistent_state.vga_attrib_reg_flip_flop =
                !self.state.persistent_state.vga_attrib_reg_flip_flop;
        }
    }

    fn sequencer_io_read(&mut self, io_address: VgaPort) -> u8 {
        if io_address == VgaPort::SEQ_INDEX_REGISTER_PORT {
            self.state.persistent_state.vga_seq_index_reg_shadow
        } else {
            let reg_address = self.state.persistent_state.vga_seq_index_reg;
            if reg_address == VgaSequencerReg::PLANE_WRITE_MASK_REGISTER {
                self.state.persistent_state.vga_sequencer_regs_shadow[reg_address]
            } else {
                self.state.persistent_state.vga_sequencer_regs[reg_address]
            }
        }
    }

    fn sequencer_io_write(&mut self, io_address: VgaPort, access_size: u16, write_data: u32) {
        if io_address == VgaPort::SEQ_INDEX_REGISTER_PORT {
            // top three bits reserved
            self.state.persistent_state.vga_seq_index_reg =
                VgaSequencerReg(write_data as u8 & 0x1F);
            self.state.persistent_state.vga_seq_index_reg_shadow = write_data as u8;

            if access_size > 1 {
                self.write_vga_sequence_reg(
                    self.state.persistent_state.vga_seq_index_reg,
                    (write_data >> 8) as u8,
                );
            }
        } else {
            self.write_vga_sequence_reg(
                self.state.persistent_state.vga_seq_index_reg,
                write_data as u8,
            );
        }
    }

    fn graphic_controller_io_read(&mut self, io_address: VgaPort) -> u8 {
        if io_address == VgaPort::GRAPHICS_INDEX_REG_PORT {
            self.state.persistent_state.vga_graphics_reg_index_shadow
        } else {
            let reg_address = self.state.persistent_state.vga_graphics_reg_index;

            match reg_address {
                VgaGraphicsReg::SET_RESET_DATA_REGISTER
                | VgaGraphicsReg::ENABLE_SET_RESET_DATA_REGISTER
                | VgaGraphicsReg::COLOR_COMPARE_REGISTER
                | VgaGraphicsReg::READ_PLANE_SELECT_REGISTER
                | VgaGraphicsReg::COLOR_DONT_CARE_REGISTER => {
                    self.state.persistent_state.vga_graphics_regs_shadow[reg_address]
                }
                _ => self.state.persistent_state.vga_graphics_regs[reg_address],
            }
        }
    }

    fn graphic_controller_io_write(
        &mut self,
        io_address: VgaPort,
        access_size: u16,
        write_data: u32,
    ) {
        if io_address == VgaPort::GRAPHICS_INDEX_REG_PORT {
            self.state.persistent_state.vga_graphics_reg_index =
                VgaGraphicsReg(write_data as u8 & 0xF);
            self.state.persistent_state.vga_graphics_reg_index_shadow = write_data as u8;
            if access_size > 1 {
                self.write_vga_graphics_control_reg(
                    self.state.persistent_state.vga_graphics_reg_index,
                    (write_data >> 8) as u8,
                );
            }
        } else {
            self.write_vga_graphics_control_reg(
                self.state.persistent_state.vga_graphics_reg_index,
                (write_data) as u8,
            );
        }
    }

    fn read_vga_port(&mut self, port_num: VgaPort) -> u8 {
        let data_read;
        match port_num {
            VgaPort::INDEX_DATA_REG_ATTR_PORT
            | VgaPort::ATTRIBUTE_READ_PORT
            | VgaPort::SEQ_INDEX_REGISTER_PORT
            | VgaPort::SEQ_DATA_REGISTER_PORT
            | VgaPort::GRAPHICS_INDEX_REG_PORT
            | VgaPort::GRAPHICS_DATA_REG_PORT => {
                // These should be handled by the specialized attribute,
                // sequencer, and GC I/O routine.
                tracing::warn!("unexpected");
                data_read = 0xFF;
            }

            VgaPort::MISC_OUTPUT_READ_PORT => {
                data_read = self.state.persistent_state.misc_output_reg;
            }

            VgaPort::INPUT_STATUS_REG0_PORT => {
                data_read = if self.is_vertical_retrace_active() {
                    0x80
                } else {
                    0
                };
            }

            VgaPort::FEATURE_CONTROL_REG_PORT => {
                // Just fudge this value. It doesn't appear to be important.
                data_read = 0x00;
            }

            VgaPort::PEL_MASK_REGISTER_PORT => {
                //
                // If this register is read more than three times,
                // we return the mode register instead of the mask
                // register
                //
                data_read = self.state.persistent_state.pel_mask_register;
            }

            VgaPort::PEL_ADDRESS_WRITE_REGISTER_PORT => {
                // Make sure we aren't supposed to read external input buffer
                if (self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_DAC_CONTROL_REGISTER]
                    & 0x4)
                    != 0
                {
                    tracing::warn!("unexpected dac state");
                }

                data_read = (self.state.persistent_state.pel_reg_write_index / 3) as u8;
            }

            VgaPort::DAC_STATUS_REGISTER_PORT => {
                // The bottom two bits are hardware-related, the upper six are always zero
                data_read = 0;
            }

            VgaPort::PEL_DATA_REGISTER_PORT => {
                let reg_address = (self.state.persistent_state.pel_reg_read_index / 3) as usize;

                // Calculate the address of the color entry we are accessing
                let pel_entry = self.state.persistent_state.pel_colors[reg_address];

                // Calculate which entry within the color we are reading
                let reg_address =
                    self.state.persistent_state.pel_reg_read_index - reg_address as u16 * 3;

                if reg_address == 0 {
                    data_read = pel_entry.red;
                } else if reg_address == 1 {
                    data_read = pel_entry.green;
                } else {
                    data_read = pel_entry.blue;
                }

                self.state.persistent_state.pel_reg_read_index += 1;
                if self.state.persistent_state.pel_reg_read_index as usize
                    >= 3 * self.state.persistent_state.pel_colors.len()
                {
                    self.state.persistent_state.pel_reg_read_index = 0;
                }
            }

            VgaPort::SUBSYSTEM_ENABLE_PORT => {
                // This is used only by IBM. It always returns 0x80 on other systems.
                data_read = 0x80;
            }

            _ => {
                data_read = 0xFF;
            }
        }

        data_read
    }

    fn write_vga_port(&mut self, port_num: VgaPort, data_to_write: u8) {
        match port_num {
            VgaPort::INDEX_DATA_REG_ATTR_PORT
            | VgaPort::SEQ_INDEX_REGISTER_PORT
            | VgaPort::SEQ_DATA_REGISTER_PORT
            | VgaPort::GRAPHICS_INDEX_REG_PORT
            | VgaPort::GRAPHICS_DATA_REG_PORT => {
                // This should be handled by the specialized function
                // for attribute, sequencer, and GC registers.
                tracing::warn!(?port_num, "unexpected vga write");
            }

            VgaPort::MISC_OUTPUT_WRITE_PORT => {
                self.state.persistent_state.misc_output_reg = data_to_write;
                self.calculate_monitor_timing();
            }

            VgaPort::SUBSYSTEM_ENABLE_PORT => {
                // Just ignore this field - not a standard port, only used by IBM
            }

            VgaPort::GRAPHICS_POS_REGISTER2_PORT | VgaPort::GRAPHICS_POS_REGISTER1_PORT => {
                //
                // These are obsolete and are here only for EGA compatibility.
                // We will ignore any writes to these locations.
                //
            }

            VgaPort::PEL_ADDRESS_WRITE_REGISTER_PORT => {
                self.state.persistent_state.pel_reg_write_index = data_to_write as u16 * 3;
            }

            VgaPort::PEL_ADDRESS_READ_REGISTER_PORT => {
                self.state.persistent_state.pel_reg_read_index = data_to_write as u16 * 3;
            }

            VgaPort::PEL_DATA_REGISTER_PORT => {
                self.write_pel_data_register(data_to_write);
            }

            VgaPort::PEL_MASK_REGISTER_PORT => {
                self.state.persistent_state.pel_mask_register = data_to_write;
            }

            _ => {
                // Do nothing...
            }
        }
    }

    fn is_vertical_retrace_active(&self) -> bool {
        let period = 1e6 / 600.; // 600 Hz refresh
        let time = self.vmtime.now();
        let us = (time.as_100ns() / 10) as f64;
        let time_into_cycle = us % period;
        time_into_cycle < 0.04 * period
    }

    fn write_crt_control_register(&mut self, reg: CrtControlReg, mut value: u8) {
        tracing::trace!(?reg, value, "write crt control register");
        let old_value = self.state.persistent_state.crt_control_regs[reg];
        if old_value == value && reg != CrtControlReg::CUSTOM_VS_GENERAL_EXTENSION_REGISTER {
            return;
        }

        if self.state.persistent_state.crt_regs_locked {
            if reg == CrtControlReg::OVERFLOW_REGISTER {
                // Only update bit 4.
                value &= 0x10;
                value |= self.state.persistent_state.crt_control_regs[reg] & !0x10;
            } else if reg < CrtControlReg::OVERFLOW_REGISTER {
                // No changes allowed.
                return;
            }
        }

        self.state.persistent_state.crt_control_regs[reg] = value;
        match reg {
            CrtControlReg::OVERFLOW_REGISTER
            | CrtControlReg::MAX_SCANLINE_REGISTER
            | CrtControlReg::LINE_COMPARE_REGISTER => {
                // We need to recalc the bit depth first
                self.calculate_graphics_mode_variables();

                // Then resize the screen based on the dot clock
                // and the current bit depth.
                self.calculate_monitor_timing();

                // Then update any other text-related variable.
                self.calculate_text_mode_variables();
            }

            CrtControlReg::START_ADDRESS_HI_REGISTER | CrtControlReg::START_ADDRESS_LO_REGISTER => {
                self.calculate_page_offset();
            }
            CrtControlReg::OFFSET_REGISTER => {
                self.calculate_line_offset_pixels();
            }

            CrtControlReg::UNDERLINE_LOCATION_REGISTER => {
                self.state.dbl_word_mode = (value & spec::CRT_UNDERLINE_MODE_DWMASK) != 0;
                self.update_access_vars();
            }

            CrtControlReg::MODE_CONTROL_REGISTER => {
                self.calculate_graphics_mode_variables();
            }

            CrtControlReg::CURSOR_START_REGISTER
            | CrtControlReg::CURSOR_END_REGISTER
            | CrtControlReg::CURSOR_LOCATION_HI_REGISTER
            | CrtControlReg::CURSOR_LOCATION_LO_REGISTER => {
                self.calculate_text_cursor_variables();
            }

            CrtControlReg::START_HORIZONTAL_BLANK_REGISTER => {
                // This register is used to determine the number
                // of text columns, so we may need to redraw if it changes.
                self.calculate_text_mode_variables();
            }

            CrtControlReg::HORIZONTAL_TOTAL_REGISTER
            | CrtControlReg::END_HORIZONTAL_BLANK_REGISTER
            | CrtControlReg::START_HORIZONTAL_RETRACE_REGISTER
            | CrtControlReg::END_HORIZONTAL_RETRACE_REGISTER
            | CrtControlReg::VERTICAL_TOTAL_REGISTER
            | CrtControlReg::PRESET_ROW_SCAN_REGISTER
            | CrtControlReg::VERTICAL_RETRACE_HI_REGISTER
            | CrtControlReg::END_VERTICAL_BLANK_REGISTER => {
                // Ignore writes to these registers
            }

            CrtControlReg::VERTICAL_RETRACE_LO_REGISTER => {
                // CRT register 0 through 7 are locked when the top bit
                // of this register is set to true.
                self.state.persistent_state.crt_regs_locked = value & 0x80 != 0;
            }

            CrtControlReg::HORIZONTAL_DISPLAY_END_REGISTER
            | CrtControlReg::VERTICAL_DISPLAY_END_REGISTER
            | CrtControlReg::START_VERTICAL_BLANK_REGISTER => {
                self.calculate_monitor_timing();
            }

            CrtControlReg::S3_DEVICE_ID_HI_REGISTER => {
                // Since this is read-only, we will replace the value
                // just written to it by the original value.
                self.state.persistent_state.crt_control_regs[reg] =
                    (spec::PCI_DEVICE_ID >> 8) as u8;
            }

            CrtControlReg::S3_DEVICE_ID_LO_REGISTER => {
                // Since this is read-only, we will replace the value
                // just written to it by the original value.
                self.state.persistent_state.crt_control_regs[reg] = spec::PCI_DEVICE_ID as u8;
            }

            CrtControlReg::S3_DEVICE_REVISION_REGISTER => {
                // Since this is read-only, we will replace the value
                // just written to it by the original value.
                self.state.persistent_state.crt_control_regs[reg] = spec::PCI_REVISION;
            }

            CrtControlReg::S3_CHIP_REVISION_NUMBER_REGISTER => {
                // Make sure we don't change the chip rev number
                self.state.persistent_state.crt_control_regs[reg] =
                    spec::S3_TRIO_CHIPSET_REV_NUMBER;
            }

            CrtControlReg::S3_MEMORY_CONFIGURATION_REGISTER => {
                // Most of the bits in this register can be ignored. We
                // need to pay attention to the following:
                //      bit 0 - use separate text page?
                //      bit 1 - two-page screen image?
                //      bit 3 - enhanced memory map (vs. VGA memory map)?
                //      bit 4-5 - bits 16-17 of start address & cursor location reg

                // Apparently, the page offset register gets zeroed
                // implicitly when the mem config register is modified.
                self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER] = 0;

                // We would normally have to call CalculatePageOffset here
                // as well, but UpdateS3State does that for us.
                self.update_s3_state();
            }

            CrtControlReg::S3_BACKWARD_COMPATIBILITY_1_REGISTER => {
                // We'll implement these if we need to
                tracing::warn!("unexpected register access");
            }

            CrtControlReg::S3_BACKWARD_COMPATIBILITY_2_REGISTER => {
                // We'll implement these if we need to, except
                // bits 5 and 3 which we can ignore
                if old_value & 0xD7 != value & 0xD7 {
                    tracing::warn!("unexpected register access");
                }
            }

            CrtControlReg::S3_REGISTER_LOCK_REGISTER => {
                // Bits 4 & 5 control locking of other CRT registers. We
                // won't honor these lock bits for now.
                // Bits 0-3 control the current 64Kb block accessed in
                // the video buffer.

                // Only allow the change if the register set is unlocked. If the registers
                // are locked (i.e. CRT register 0x38 ANDed with 0xCC does not equal
                // 0x48), we need to put the old 0x35 register value back. At least
                // one VESA driver uses this to verify it is dealing with an S3 chip.

                if self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_UNLOCK_VGA_REGISTERS_1_REGISTER]
                    & 0xCC
                    == 0x48
                {
                    self.update_access_vars();
                } else {
                    self.state.persistent_state.crt_control_regs[reg] = old_value;
                }
            }

            CrtControlReg::S3_UNLOCK_VGA_REGISTERS_1_REGISTER => {
                //Currently, only use is above (CrtControlReg::S3_REGISTER_LOCK_REGISTER => {
                // as in this case the extraneous bits are "don't care", not "reserved", we should probably
                // preserve them in the register and do the masking on use.  But if we were using it a lot,
                // it might make sense to do the mask here once and for all.
            }

            CrtControlReg::S3_UNLOCK_VGA_REGISTERS_2_REGISTER => {}

            CrtControlReg::S3_CONFIGURATION_1_REGISTER => {
                // Bits 0-1 are read-only. We need to replace them to
                // indicate the card is PCI-based.
                self.state.persistent_state.crt_control_regs[reg] =
                    (value & 0xFC) | (old_value & 0x3);
                self.update_s3_state();
            }

            CrtControlReg::S3_CONFIGURATION_2_REGISTER => self.update_s3_state(),

            CrtControlReg::S3_BACKWARD_COMPATIBILITY_3_REGISTER
            | CrtControlReg::S3_MISC_1_REGISTER
            | CrtControlReg::S3_DATA_TRANSFER_REGISTER
            | CrtControlReg::S3_INTERLACE_START_REGISTER
            | CrtControlReg::S3_EXTENDED_BIOS_FLAG_1_REGISTER
            | CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_2_REGISTER
            | CrtControlReg::S3_EXTERNAL_SYNC_CONTROL_1_REGISTER
            | CrtControlReg::S3_EXTERNAL_SYNC_CONTROL_2_REGISTER => {
                // These registers control hardware-related functions
                // which we can completely ignore in our emulation.
            }

            CrtControlReg::S3_SYSTEM_CONFIGURATION_REGISTER => {
                // In general, we can ignore everything involved with
                // this register. It controls memory wait states, etc.
                // The only bit we need to be careful with is bit 0 which
                // controls the locking/unlocking of the other enahnced
                // registers (0x40 and above). We will ignore this for
                // now, but may need to implement it in the future.
            }

            CrtControlReg::S3_MODE_CONTROL_REGISTER => self.calculate_monitor_timing(),

            CrtControlReg::S3_EXTENDED_MODE_REGISTER => self.calculate_line_offset_pixels(),

            CrtControlReg::S3_HW_CURSOR_DEAD_1
            | CrtControlReg::S3_HW_CURSOR_DEAD_2
            | CrtControlReg::S3_HW_CURSOR_DEAD_3
            | CrtControlReg::S3_HW_CURSOR_DEAD_4
            | CrtControlReg::S3_HW_CURSOR_DEAD_5
            | CrtControlReg::S3_HW_CURSOR_DEAD_6
            | CrtControlReg::S3_HW_CURSOR_DEAD_7
            | CrtControlReg::S3_HW_CURSOR_DEAD_8
            | CrtControlReg::S3_HW_CURSOR_DEAD_9
            | CrtControlReg::S3_HW_CURSOR_DEAD_10
            | CrtControlReg::S3_HW_CURSOR_DEAD_11 => {
                tracing::warn!("hw cursor accessed");
            }

            CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER => {
                // Bits 0-3 can be ignored. The following are important:
                //      bit 4-5     - pixel length (1, 2, reserved, 4 bytes)
                //      bit 0/6-7   - graphics engine screen width
                self.update_s3_state();
            }

            CrtControlReg::S3_GENERAL_OUTPUT_REGISTER => {
                // Bits 0-3 are read-only, so we will put them back to their old value.
                self.state.persistent_state.crt_control_regs[reg] &= 0xF0;
                self.state.persistent_state.crt_control_regs[reg] |= old_value & 0x0F;
            }

            CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_CONTROL_REGISTER => {
                // We can ignore most of the bits in this register. Several
                // are important:
                //      bit 0-1 - linear address window size (64k, 1Mb, 2Mb, 4Mb)
                //      bit 4   - enable linear addressing
                self.s3_setup_linear_address_window();
                self.update_access_vars();
            }

            CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER => {
                self.calculate_page_offset();
                self.update_access_vars();
                self.calculate_line_offset_pixels();
            }

            CrtControlReg::S3_EXTENDED_DAC_CONTROL_REGISTER => {
                // This used to tell us if the hardware cursor was in Windows mode or X11.
                // With no more hardware cursor, we don't do anything with this.
            }

            CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_1_REGISTER
            | CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_2_REGISTER => {
                let new_base = (self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_2_REGISTER]
                    as u64
                    | ((self.state.persistent_state.crt_control_regs
                        [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_1_REGISTER]
                        as u64)
                        << 8))
                    * 64
                    * 1024;
                self.s3_set_linear_address_base(new_base);
            }

            CrtControlReg::S3_EXTENDED_BIOS_FLAG_2_REGISTER
            | CrtControlReg::S3_EXTENDED_BUS_GRANT_REGISTER => {
                tracing::warn!(?reg, value, "unexpected access");
            }

            CrtControlReg::S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER
            | CrtControlReg::S3_EXTENDED_HORIZONTAL_OVERFLOW_REGISTER => {
                self.calculate_monitor_timing();
            }

            CrtControlReg::S3_BIOS_FLAG_REGISTER
            | CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_3_REGISTER
            | CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_5_REGISTER
            | CrtControlReg::S3_EXTENDED_MISC_CONTROL_0_REGISTER
            | CrtControlReg::S3_CONFIGURATION_3_REGISTER
            | CrtControlReg::S3_EXTENDED_BIOS_FLAG_3_REGISTER
            | CrtControlReg::S3_EXTENDED_BIOS_FLAG_4_REGISTER
            | CrtControlReg::S3_EXTENDED_BIOS_FLAG_5_REGISTER
            | CrtControlReg::S3_EXTENDED_BIOS_FLAG_6_REGISTER
            | CrtControlReg::CONFIGURATION_4_REGISTER => {
                // We don't need to support these
            }

            CrtControlReg::S3_EXTENDED_MEMORY_CONTROL_4_REGISTER => {
                // Bits 5-6 control byte-swapping (?). Other bits are always zero.
                if value & 0x60 != 0 {
                    tracing::warn!(?reg, value, "unexpected bit");
                }

                self.state.persistent_state.crt_control_regs[reg] &= 0x60;
            }

            CrtControlReg::S3_EXTENDED_MISC_CONTROL_1_REGISTER => {
                // The low-order bit of this register is the same as
                // that of 0x4AE8 and controls the enabling of enhanced mode.
                // None of the other bits are interesting.
                self.state.persistent_state.s3.adv_function_control_reg &= !0x0001;
                self.state.persistent_state.s3.adv_function_control_reg |= value as u16 & 0x01;
                self.calculate_graphics_mode_variables();
            }

            CrtControlReg::S3_EXTENDED_MISC_CONTROL_2_REGISTER => {
                // Bits 4-7 control the color mode. Bits 2-3 control
                // the "streams processor" which is not available on
                // the Trio.
                self.calculate_graphics_mode_variables();

                // The dot-clock routine uses the bits-per-pixel
                // setting which is adjusted by RecalcGraphicsModeVars
                // above. So we may need to readjust it.
                self.calculate_monitor_timing();
            }

            CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER => {
                // This is an alternate way of setting the display
                // start address (at which pixels are to be displayed).
                // If this value is zero, the value is obtained using
                // the older mechanism where bits are scattered in a
                // number of registers. If non-zero, these older
                // registers are ignored.
                // NOTE:  The spec says that only the low 4 bits count.  But see NOTE in
                // CalculatePageOffset().  --richyam
                self.state.persistent_state.crt_control_regs[reg] &= 0x3F;
                self.state.persistent_state.crt_control_regs_shadow[reg] = value;
                self.calculate_page_offset();
            }

            CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER => {
                // This is an alternate way of setting the CPU base
                // address (used to access windows within the display
                // RAM). Similar to 0x69 above.

                // other bits are reserved
                self.state.persistent_state.crt_control_regs[reg] &= 0x3F;
                self.state.persistent_state.crt_control_regs_shadow[reg] = value;
                self.update_access_vars();
            }

            CrtControlReg::CUSTOM_VS_1_REGISTER | CrtControlReg::CUSTOM_VS_2_REGISTER => {
                // These two registers are used to deal with large resolutions above
                // the 1600x1200 screen size.
                self.update_s3_state();
            }

            CrtControlReg::CUSTOM_VS_BIOS_LOGO_REGISTER => {
                // This register is used by the BIOS to enable and disable a
                // special "BIOS Logo" mode. When enabled, all video comes
                // from our private splash screen instead of vram.
                self.record_video_mode_change();
                self.force_screen_redraw();
            }

            CrtControlReg::CUSTOM_VS_GENERAL_EXTENSION_REGISTER => {
                self.handle_general_extension_register_write(value);
            }

            _ => {}
        }
    }

    /// Handles writes to the general extension register, which handles certain special
    /// needs:
    ///
    /// The bios needs a way to clear the screen.  It used to use the S3 accelerator,
    /// but we took that out, since it was complicated and burdensome to maintain, while
    /// being slow and undesirable.  But the bios still needs a way to clear the screen,
    /// so we gave it this little backdoor.
    ///
    /// As of win8, the synthvid vsc needs a way to use this device to handle crash blue
    /// screens.
    fn handle_general_extension_register_write(&mut self, requested_operation: u8) {
        match requested_operation {
            spec::BIOS_CLEAR_SCREEN_CODE => {
                // The way the old bios code drove the accelerator for this, it ended up
                // zeroing all of vram, so we shall just do that directly.
                self.vram
                    .fill_at(0, 0, self.vram_size as usize)
                    .expect("framebuffer is mapped");
            }

            spec::SYNTHVID_BLUE_SCREEN_CODE => {
                // Handles a request from the synthvid vsc to prepare for handling a crash
                // blue screen.  This means switching modes and becoming the active video.
                // Then the vsc can drive the blue screen, because we share vram.  (It can't
                // drive it through the VideoSynthDevice because that requires vmbus, which
                // isn't functional during a guest crash).
                self.do_blue_screen_mode_change();
                self.suppress_initial_activation = false;
                self.request_activation();
            }

            _ => {
                // Anything we don't expect just gets ignored.
            }
        }
    }

    fn write_vga_sequence_reg(&mut self, reg_index: VgaSequencerReg, new_value: u8) {
        let old_reg_value = self.state.persistent_state.vga_sequencer_regs[reg_index];

        if old_reg_value != new_value {
            self.state.persistent_state.vga_sequencer_regs[reg_index] = new_value;

            match reg_index {
                VgaSequencerReg::RESET_REGISTER => {
                    // Do nothing.. we don't need to do a hardware clock reset here
                }

                VgaSequencerReg::CLOCKING_MODE_REGISTER => {
                    // Check to see whether we're turning the video on or off. Did state change?
                    if (old_reg_value ^ new_value) & 0x20 != 0 {
                        self.state.persistent_state.video_enabled = (0x20 & new_value) == 0;
                        self.force_screen_redraw();

                        /* TODO
                        if self.state.PersistentState.VideoEnabled && todo!("!m_IsTheActiveVideo") {
                            self.RequestActivation();
                        }
                        */
                    }

                    // If they are changing the dot clock value, we may need
                    // to pixel double.
                    if (old_reg_value ^ new_value) & 0x08 != 0 {
                        self.calculate_monitor_timing();
                    }
                }

                VgaSequencerReg::PLANE_WRITE_MASK_REGISTER => {
                    self.state.persistent_state.vga_sequencer_regs_shadow[reg_index] = new_value;
                    let masked = new_value & 0xF;
                    self.state.persistent_state.vga_sequencer_regs[reg_index] = masked;

                    if self.state.plane_write_mask != masked {
                        self.state.plane_write_mask = masked;
                        self.update_access_vars();
                    }
                }

                VgaSequencerReg::CHARACTER_FONT_SELECT_REGISTER => {
                    self.calculate_text_mode_variables();
                }

                VgaSequencerReg::MEMORY_MODE_CONTROL_REGISTER => {
                    self.state.chain_4_mode = (new_value & spec::SEQ_MEM_MODE_CHAIN4_MASK) != 0;
                    self.state.odd_even_mode = (new_value & spec::SEQ_MODE_ODD_EVEN_MASK) == 0;
                    self.update_access_vars();
                    self.calculate_graphics_mode_variables();
                    self.calculate_monitor_timing();
                }

                VgaSequencerReg::UNLOCK_S3_EXTENDED_SEQUENCER_REGISTERS_REGISTER => {
                    // We'll ignore the locking/unlocking for now
                }

                //
                // Old comment:
                // All of these registers are new on the trio-64 and did not exist on
                // the 928. These registers aren't set properly by the current 928 BIOS,
                // so we'll set their values, but ignore them. This MUST change if we
                // get a proper trio 64 BIOS. See SetSVGADotClock for more details.

                // Seems to be dated, because the original registered cased here
                // Didn't even correspond to what's in the Trio64V docs.  And there appears
                // to no longer be any "SetSVGADotClock" to see.  An old comment also
                // suggested that Corel Linux installer accesses register 10, but I'm not sure
                // we're even talking about the same registers anymore.  In any case, we
                // continue to quietly do nothing with these:
                //
                VgaSequencerReg::UNSUPPORTED_09_REGISTER
                | VgaSequencerReg::UNSUPPORTED_0A_REGISTER
                | VgaSequencerReg::UNSUPPORTED_0B_REGISTER
                | VgaSequencerReg::UNSUPPORTED_0D_REGISTER
                | VgaSequencerReg::UNSUPPORTED_10_REGISTER
                | VgaSequencerReg::UNSUPPORTED_11_REGISTER
                | VgaSequencerReg::UNSUPPORTED_12_REGISTER
                | VgaSequencerReg::UNSUPPORTED_13_REGISTER
                | VgaSequencerReg::UNSUPPORTED_14_REGISTER
                | VgaSequencerReg::UNSUPPORTED_15_REGISTER
                | VgaSequencerReg::UNSUPPORTED_16_REGISTER
                | VgaSequencerReg::UNSUPPORTED_17_REGISTER
                | VgaSequencerReg::UNSUPPORTED_18_REGISTER
                | VgaSequencerReg::UNSUPPORTED_1C_REGISTER => {}

                _ => {
                    // non-existent register
                }
            }
        }
    }

    fn write_vga_attribute_reg(&mut self, attrib_reg: VgaAttribReg, new_value: u8) {
        let old_value = self.state.persistent_state.vga_attrib_regs[attrib_reg];

        if old_value != new_value {
            self.state.persistent_state.vga_attrib_regs[attrib_reg] = new_value;

            if (attrib_reg >= VgaAttribReg::PALETTE_0_REGISTER)
                && (attrib_reg <= VgaAttribReg::PALETTE_F_REGISTER)
            {
                // Colors could have changed, so force complete redraw
                self.mark_palette_dirty();
            } else {
                match attrib_reg {
                    VgaAttribReg::MODE_CONTROL_REGISTER => {
                        let was_text_mode = self.state.persistent_state.text_mode;

                        self.state.persistent_state.text_mode =
                            (new_value & spec::ATTRIBUTE_CONTROLLER_MODE_CONTROL_AG_MASK) == 0;

                        // This can change the color attributes, so force redraw
                        self.force_screen_redraw();

                        if was_text_mode != self.state.persistent_state.text_mode {
                            self.record_video_mode_change();
                            self.mark_palette_dirty();
                        }

                        // Recompute text mode variables to handling blinking and mono modes
                        if self.state.persistent_state.text_mode {
                            // This call also calls SetLegacyWritesTimer() - this is
                            // potentially needed to get direct text mode memory block
                            // write notifications set up.
                            self.calculate_text_mode_variables();
                        }
                    }

                    VgaAttribReg::COLOR_PLANE_ENABLE_REGISTER
                    | VgaAttribReg::PIXEL_PADDING_REGISTER => {
                        // These can change the color attributes, so force redraw
                        self.mark_palette_dirty();
                        self.calculate_graphics_mode_variables();
                    }

                    VgaAttribReg::HORIZONTAL_PIXEL_PANNING_REGISTER => {
                        // This can affect scrolling; force a redraw
                        self.force_screen_redraw();
                    }

                    VgaAttribReg::VGA_EXTENSION_REGISTER_16 => {
                        //
                        // Must not allow bit 4 of this register to flip or the
                        // Tseng Labs ET4000 driver (under Windows 2000) will think that
                        // this is an ET4000 chip.  Note that I have no idea what this
                        // register (which previously wasn't even defined in this code
                        // base) actually "does" in the mind of a 1995-era VGA board
                        // designer.  Presumably it does nothing on non-Tseng parts,
                        // or the Tseng driver wouldn't believe so strongly that
                        // writability implies ownership.
                        //

                        self.state.persistent_state.vga_attrib_regs[attrib_reg] &= !(1 << 4);
                    }
                    _ => {}
                }
            }
        }
    }

    fn write_vga_graphics_control_reg(&mut self, graphics_reg: VgaGraphicsReg, new_value: u8) {
        let old_value = self.state.persistent_state.vga_graphics_regs[graphics_reg];

        if old_value != new_value {
            self.state.persistent_state.vga_graphics_regs[graphics_reg] = new_value;

            match graphics_reg {
                VgaGraphicsReg::SET_RESET_DATA_REGISTER => {
                    let masked = new_value & 0xF;
                    self.state.set_reset_value = masked;
                    self.state.persistent_state.vga_graphics_regs[graphics_reg] = masked;
                    self.state.persistent_state.vga_graphics_regs_shadow[graphics_reg] = new_value;
                    self.update_access_vars();
                }

                VgaGraphicsReg::ENABLE_SET_RESET_DATA_REGISTER => {
                    let masked = new_value & 0xF;
                    self.state.set_reset_mask = masked;
                    self.state.persistent_state.vga_graphics_regs[graphics_reg] = masked;
                    self.state.persistent_state.vga_graphics_regs_shadow[graphics_reg] = new_value;
                    self.update_access_vars();
                }

                VgaGraphicsReg::COLOR_COMPARE_REGISTER => {
                    let masked = new_value & 0xF;
                    self.state.color_compare_value = masked;
                    self.state.persistent_state.vga_graphics_regs[graphics_reg] = masked;
                    self.state.persistent_state.vga_graphics_regs_shadow[graphics_reg] = new_value;
                    // Compute the color compare mask
                    self.state.color_compare_mask32 = expand_mask(self.state.color_compare_value);
                }

                VgaGraphicsReg::RASTER_OP_ROTATE_COUNT_REGISTER => {
                    // Convert from rotate right to rotate left value
                    self.state.data_rotate_value = new_value & 0x7;
                    if self.state.data_rotate_value != 0 {
                        self.state.data_rotate_value = 8 - self.state.data_rotate_value;
                    }

                    self.state.function_select = (new_value >> 3) & 0x3;
                    self.update_access_vars();
                }

                VgaGraphicsReg::READ_PLANE_SELECT_REGISTER => {
                    let masked = new_value & 0x3;
                    self.state.plane_read_num = masked;
                    self.state.persistent_state.vga_graphics_regs[graphics_reg] = masked;
                    self.state.persistent_state.vga_graphics_regs_shadow[graphics_reg] = new_value;
                }

                VgaGraphicsReg::MODE_REGISTER => {
                    self.state.write_mode = new_value & 0x3;
                    self.state.read_mode = (new_value >> 3) & 0x1;

                    // If the color shift value changed, we also need to
                    // recalculate the palette.
                    if ((new_value ^ old_value) & 0x60) != 0 {
                        self.calculate_graphics_mode_variables();
                        self.calculate_monitor_timing();
                    }
                }

                VgaGraphicsReg::MEMORY_MAP_MODE_CONTROL_REGISTER => {
                    self.calculate_vga_address_range();
                    self.update_access_vars();
                }

                VgaGraphicsReg::COLOR_DONT_CARE_REGISTER => {
                    let masked = new_value & 0xF;
                    self.state.color_dont_care = masked;
                    self.state.persistent_state.vga_graphics_regs[graphics_reg] = masked;
                    self.state.persistent_state.vga_graphics_regs_shadow[graphics_reg] = new_value;

                    // Compute the color-don't-care mask
                    self.state.color_dont_care_mask32 = !expand_mask(self.state.color_dont_care);
                }

                VgaGraphicsReg::BIT_MASK_REGISTER => {
                    let v = new_value as u32;
                    self.state.pixel_mask32 = v | (v << 8) | (v << 16) | (v << 24);
                }
                _ => {}
            }
        }
    }

    /// This routine recalculates any access variables derived from
    /// other access variables. It should be called when any of
    /// the other values change.
    fn update_access_vars(&mut self) {
        // The "window" or 64Kb chunk we can currently address is stored in
        // two different CRT registers - unless they are overridden by a
        // newer mechanism which stores the entire address in a single register.

        assert_eq!(
            self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER]
                & !0x3F,
            0
        );

        let mut new_video_offset = self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_4_REGISTER]
            as u32;

        if new_video_offset == 0 || !self.state.persistent_state.s3.enhanced_mode {
            new_video_offset = (self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_REGISTER_LOCK_REGISTER] as u32
                & 0xF)
                | ((self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER]
                    as u32
                    & 0xC)
                    << 2);
        }

        // Multiply by 64Kb to get the requested window base.
        new_video_offset <<= 16;

        if self.state.video_window_offset != new_video_offset {
            self.state.video_window_offset = new_video_offset;

            self.s3_setup_linear_address_window();
        }

        if self.state.persistent_state.s3.enhanced_mode {
            self.state.dbl_word_mode = false;
        } else {
            self.state.dbl_word_mode = (self.state.persistent_state.crt_control_regs
                [CrtControlReg::UNDERLINE_LOCATION_REGISTER]
                & spec::CRT_UNDERLINE_MODE_DWMASK)
                != 0;

            let pixel_mask = self.state.persistent_state.vga_graphics_regs
                [VgaGraphicsReg::BIT_MASK_REGISTER] as u32;

            self.state.pixel_mask32 =
                pixel_mask | (pixel_mask << 8) | (pixel_mask << 16) | (pixel_mask << 24);

            self.state.plane_write_mask32 = expand_mask(self.state.plane_write_mask);
            self.state.plane_3_in_use = (self.state.plane_write_mask32 & 0xFF000000) != 0;
            self.state.set_reset_mask32 = expand_mask(self.state.set_reset_mask);
            self.state.set_reset_value32 = expand_mask(self.state.set_reset_value);

            // Mask out unenabled set/reset fields
            self.state.set_reset_mask_value32 =
                self.state.set_reset_value32 & self.state.set_reset_mask32;
        }
    }

    /// This routine is called when a new linear base register is written to the
    /// PCI card's base address register.
    fn s3_set_linear_address_base(&mut self, new_base: u64) {
        // Convert to 64Kb block index
        let converted_base = (new_base / (64 * 1024)) as u32;

        if new_base != self.state.persistent_state.s3.linear_addr_window as u64 {
            // Write the value to the S3 registers
            self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_2_REGISTER] =
                converted_base as u8;

            self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_POSITION_1_REGISTER] =
                (converted_base >> 8) as u8;

            self.state.persistent_state.s3.linear_addr_window = new_base as u32;

            // Change the mapping
            self.s3_setup_linear_address_window();
        }
    }

    /// This routine should be called when the base or window offset
    /// of the linear address is changing. It remaps the emulated
    /// linear frame buffer as necessary to address the specified
    /// window within VRAM.
    fn s3_setup_linear_address_window(&mut self) {
        let use_linear_addr = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_CONTROL_REGISTER]
            & 0x10)
            != 0;

        let window_size_code = self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_LINEAR_ADDRESS_WINDOW_CONTROL_REGISTER]
            & 0x3;

        let mut window_size = if window_size_code == 0 {
            64 * 1024
        } else {
            (1 << (window_size_code - 1)) * (1024 * 1024)
        };

        // Limit the actual window size to the size of VRAM
        if window_size > self.vram_size {
            window_size = self.vram_size;
        }

        // The linear base window is always scaled by 64Kb, but must be aligned to the window size.
        let mut window_base = self.state.persistent_state.s3.linear_addr_window;
        window_base &= !(window_size - 1);

        //
        // It's generally a bad mistake to set the linear frame
        // buffer address to zero when feature is enabled!
        //
        assert!(!(window_base == 0 && use_linear_addr));

        if use_linear_addr {
            self.control.map(
                window_base.into(),
                Some(MemoryRange::new(0..window_size.into())),
            );
        } else {
            self.control.unmap();
        }

        self.state.persistent_state.s3.addr_window_offset = self.state.video_window_offset;
        self.state.persistent_state.s3.linear_mapping = use_linear_addr;
        self.state.persistent_state.s3.addr_window_size = window_size;
        self.state.persistent_state.s3.addr_window_base = window_base;
    }

    /// This function recalculates the internal graphics-mode variables when ever
    /// the mode registers are altered.
    fn calculate_graphics_mode_variables(&mut self) {
        let current_dac_mode = self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_MISC_CONTROL_2_REGISTER]
            & 0xF0;

        let new_bits_per_pixel;

        // Are we using enhanced-mode features?
        if (self.state.persistent_state.s3.adv_function_control_reg & 0x1) != 0 {
            self.state.persistent_state.enhanced_dac_mode = true;

            if current_dac_mode == 0x70 {
                tracing::warn!("We don't support 24-bpp mode");
            }

            if current_dac_mode == 0xD0 {
                new_bits_per_pixel = 32;
            } else if current_dac_mode == 0x50 {
                new_bits_per_pixel = 16;
            } else if current_dac_mode == 0x30 {
                new_bits_per_pixel = 15;
            } else {
                new_bits_per_pixel = 8;
            }
        } else {
            self.state.persistent_state.enhanced_dac_mode = false;

            // Shift mode 2/3 indicates we should use 256 colors, 1 indicates
            // 4 colors and 0 indicates 16 colors.
            let shift_reg_value =
                (self.state.persistent_state.vga_graphics_regs[VgaGraphicsReg::MODE_REGISTER] >> 5)
                    & 0x3;

            if shift_reg_value >= 2 {
                new_bits_per_pixel = 8;
            } else if shift_reg_value == 1 {
                new_bits_per_pixel = 2;
            } else if (self.state.persistent_state.crt_control_regs
                [CrtControlReg::MODE_CONTROL_REGISTER]
                & 0x20)
                == 0
                && self.state.persistent_state.vga_attrib_regs
                    [VgaAttribReg::COLOR_PLANE_ENABLE_REGISTER]
                    == 1
            {
                new_bits_per_pixel = 1;
            } else {
                new_bits_per_pixel = 4;
            }
        }

        // If the bits-per-pixel changed, we need to force a complete redraw.
        if self.state.persistent_state.bits_per_pixel != new_bits_per_pixel {
            self.state.persistent_state.bits_per_pixel = new_bits_per_pixel;
            self.record_video_mode_change();
            self.mark_palette_dirty();
            self.calculate_monitor_timing();
        }

        // Get the line compare value.
        let new_line_compare = self.get_line_compare_value();

        if new_line_compare != self.state.persistent_state.line_compare_value {
            self.state.persistent_state.line_compare_value = new_line_compare;
            self.force_screen_redraw();
        }
    }

    /// This function sets a new dot clock value which defines the visible size of
    /// the screen.
    ///
    /// We actually ignore the clock timings completely.  The clock timing is
    /// usually just calculated as:
    ///
    /// Horizontal Total * Vertical Total * refresh rate.
    ///
    /// Since the Horizontal Total and Vertical Total are mostly just used to
    /// configure a real monitor, those values aren't too useful to us.  The real
    /// mechanism we use to determine the size of the visible display are the
    /// Horizontal and Vertical End registers.
    ///
    /// Under normal circumstances, we can read these values directly and calculate
    /// the size of the display from them.  However, there are at least two special
    /// cases that the Linux drivers use.  As of yet, I haven't been able to find a
    /// real piece of Trio64 that documents these, so we just assume that the Linux
    /// driver is correct for now, and we'll keep hunting for better documentation.
    /// Our custom-hacked SVGA bios now uses these assumptions when configuring the
    /// registers.
    ///
    /// In addition, CGA modes are also special cased.
    fn calculate_monitor_timing(&mut self) {
        // The vertical limit is stored in three different registers.
        let mut vertical_end_reg = 1 + self.state.persistent_state.crt_control_regs
            [CrtControlReg::VERTICAL_DISPLAY_END_REGISTER]
            as u16;

        if self.state.persistent_state.crt_control_regs[CrtControlReg::OVERFLOW_REGISTER] & (1 << 1)
            != 0
        {
            vertical_end_reg += 256;
        }
        if self.state.persistent_state.crt_control_regs[CrtControlReg::OVERFLOW_REGISTER] & (1 << 6)
            != 0
        {
            vertical_end_reg += 512;
        }
        if self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER]
            & (1 << 1)
            != 0
        {
            vertical_end_reg += 1024;
        }

        // In some cases (e.g. The Incredible Machine) they set up the display end registers
        // but they also set the vertical blanking register to a value lower than the display
        // end.  On real hardware, I think this would result in a black area drawn for the
        // rest of the screen.  In our case (taken from earlier VPC versions), we just
        // shorten the screen.
        let vertical_blank_reg = 1
            + (self.state.persistent_state.crt_control_regs
                [CrtControlReg::START_VERTICAL_BLANK_REGISTER] as u16
                | ((self.state.persistent_state.crt_control_regs[CrtControlReg::OVERFLOW_REGISTER]
                    as u16
                    & (1 << 3))
                    << 5)
                | ((self.state.persistent_state.crt_control_regs
                    [CrtControlReg::MAX_SCANLINE_REGISTER] as u16
                    & (1 << 5))
                    << 4)
                | ((self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER]
                    as u16
                    & (1 << 2))
                    << 8));

        if vertical_blank_reg < vertical_end_reg {
            vertical_end_reg = vertical_blank_reg - 1;
        }

        // The horizontal limit is stored in two different registers.  The final
        // result is multiplied by 8 because we're scanning out 8 pixels each time
        // -- this is related to text scanning, but the graphics hardware works the
        // same way.

        let mut horizontal_end_reg = 1 + self.state.persistent_state.crt_control_regs
            [CrtControlReg::HORIZONTAL_DISPLAY_END_REGISTER]
            as u16;

        if self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_HORIZONTAL_OVERFLOW_REGISTER]
            & (1 << 1)
            != 0
        {
            horizontal_end_reg += 256;
        }

        horizontal_end_reg *= 8;

        let mut new_width = horizontal_end_reg;
        let mut new_height = vertical_end_reg;

        // Linux special case #1
        // When the depth is 16 on a Trio 64, the Linux driver doubles all of the
        // horizontal register values.
        if self.state.persistent_state.bits_per_pixel == 16 {
            new_width /= 2;
        } else if self.state.persistent_state.bits_per_pixel == 2 {
            // CGA mode
            new_width *= 2;
        } else if self.state.persistent_state.bits_per_pixel == 4
            && (self.state.persistent_state.vga_sequencer_regs
                [VgaSequencerReg::CLOCKING_MODE_REGISTER]
                & 0x08)
                != 0
        {
            // EGA/VGA mode
            new_width *= 2;
        }

        // Linux special case #2
        // When setting up an interlaced mode, the Linux driver only uses half
        // the normal vertical register values.
        if self.state.persistent_state.crt_control_regs[CrtControlReg::S3_MODE_CONTROL_REGISTER]
            & (1 << 5)
            != 0
        {
            new_height *= 2;
        }

        // Make sure we don't use an errantly large width or height.
        // Note that max value for newWidth is 4096 (following max-case paths to
        // this point), and max value for newHeight is 2048, so the below
        // multiplication does not overflow.

        if (new_width as u32 * new_height as u32 * (self.get_current_video_depth() / 8) as u32)
            > self.vram_size
        {
            // totally bogus setting, ignore entirely
            return;
        }

        // Did any of the values change?
        let mut mode_changed = false;
        if self.state.persistent_state.pcvideo_width != new_width {
            self.state.persistent_state.pcvideo_width = new_width;
            mode_changed = true;
        }

        if self.state.persistent_state.pcvideo_height != new_height {
            self.state.persistent_state.pcvideo_height = new_height;
            mode_changed = true;
        }

        if self.state.persistent_state.adj_pcvideo_height != new_height {
            self.state.persistent_state.adj_pcvideo_height = new_height;
            mode_changed = true;
        }

        if mode_changed {
            if self.state.persistent_state.text_mode {
                self.calculate_text_mode_variables();
            }

            self.record_video_mode_change();
        }
    }

    /// This function recalculates the internal text-mode variables when
    /// ever the mode registers are altered.
    fn calculate_text_mode_variables(&mut self) {
        // Determine the new base of the text mode video buffer
        let temp_byte = self.state.persistent_state.vga_sequencer_regs
            [VgaSequencerReg::CHARACTER_FONT_SELECT_REGISTER];

        let old_value = self.text.char_set_1;
        self.text.char_set_1 = ((temp_byte >> 1) & 0x6) | ((temp_byte >> 5) & 0x1);

        if self.text.char_set_1 != old_value {
            self.force_screen_redraw();
        }

        let old_value = self.text.char_set_2;
        self.text.char_set_2 = ((temp_byte << 1) & 0x6) | ((temp_byte >> 4) & 0x1);

        if self.text.char_set_2 != old_value {
            self.force_screen_redraw();
        }

        self.text.character_set_512 = self.text.char_set_1 != self.text.char_set_2;

        // Set the text font height (assume it is always at least 10 pixels)
        let old_value = self.text.text_char_height;

        let temp_byte = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::MAX_SCANLINE_REGISTER]
            & 0x1F)
            + 1;

        // Make sure we are using a screen height within reason
        let mut screen_rows = self.state.persistent_state.pcvideo_height;
        if !(320..=480).contains(&screen_rows) {
            screen_rows = 400;
        }

        if temp_byte >= 8 {
            self.text.text_char_height = temp_byte;
        } else {
            self.text.text_char_height = spec::DEFAULT_VGA_CHARACTER_HEIGHT;
        }

        if self.text.text_char_height != old_value {
            // Force a resizing of the window if necessary
            // This also does a redraw
            self.record_video_mode_change();
        }

        //
        // Make sure we don't overflow some of our internal arrays -
        // limit the row count to match our assumptions.
        //
        let old_value = self.text.text_rows;
        self.text.text_rows = (screen_rows / self.text.text_char_height as u16) as u8;
        if self.text.text_rows > spec::TOTAL_VGA_MAX_TEXT_ROWS {
            self.text.text_rows = spec::TOTAL_VGA_MAX_TEXT_ROWS;
        }

        if self.text.text_rows != old_value {
            // Force a resizing of the window if necessary
            // This also does a redraw
            self.record_video_mode_change();
        }

        let is_lo_res = self.state.persistent_state.crt_control_regs
            [CrtControlReg::START_HORIZONTAL_BLANK_REGISTER]
            <= spec::TOTAL_VGA_LORES_TEXT_COLUMNS;
        if is_lo_res != self.text.lo_res_text_mode {
            // If we are switching from lo to hi res, or vice versa, force redraw
            self.record_video_mode_change();
        }

        self.text.lo_res_text_mode = is_lo_res;
        self.text.current_text_columns = if is_lo_res {
            spec::TOTAL_VGA_LORES_TEXT_COLUMNS.into()
        } else {
            spec::TOTAL_VGA_HIRES_TEXT_COLUMNS.into()
        };

        self.text.text_char_width = if is_lo_res {
            spec::VGA_LORES_CHARACTER_WIDTH
        } else {
            spec::VGA_HIRES_CHARACTER_WIDTH
        };

        // check for blinking
        let attr_mode_control =
            self.state.persistent_state.vga_attrib_regs[VgaAttribReg::MODE_CONTROL_REGISTER];

        self.text.blinking_enabled = (attr_mode_control & 0x08) != 0;
        self.evaluate_text_blink_timer_active_status();
        self.text.mono_text_mode = (attr_mode_control & 0x02) != 0;

        self.calculate_text_cursor_variables();
    }

    /// This function recalculates the internal cursor variables when
    /// ever the cursor changes position or other video parameters change.
    fn calculate_text_cursor_variables(&mut self) {
        self.text.draw_text_cursor = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::CURSOR_START_REGISTER]
            & spec::CURSOR_ENABLED_FLAG)
            == 0;

        self.evaluate_cursor_blink_timer_active_status();

        let start_scan_line = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::CURSOR_START_REGISTER]
            & spec::CURSOR_SCAN_LINE_MASK) as u16;

        let end_scan_line = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::CURSOR_END_REGISTER]
            & spec::CURSOR_SCAN_LINE_MASK) as u16;

        let char_height = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::MAX_SCANLINE_REGISTER]
            & spec::CRT_MAX_SCAN_LINE_MASK) as u16
            + 1;

        // Calculate cursor offset and height
        self.text.cursor_first_scanline = start_scan_line;

        let cursor_height = if end_scan_line >= start_scan_line && start_scan_line <= char_height {
            end_scan_line - start_scan_line + 1
        } else {
            0
        };

        // Finally, calculate the last line of the cursor.
        self.text.cursor_last_scanline = self.text.cursor_first_scanline + cursor_height;
        self.text.cursor_first_scanline += 1;

        let mut cursor_loc = (((self.state.persistent_state.crt_control_regs
            [CrtControlReg::CURSOR_LOCATION_HI_REGISTER] as u16)
            << 8)
            & 0x3F00)
            | self.state.persistent_state.crt_control_regs
                [CrtControlReg::CURSOR_LOCATION_LO_REGISTER] as u16;

        cursor_loc =
            cursor_loc.saturating_sub(self.state.persistent_state.cur_page_start_offset as u16);

        // Calculate the current cursor row/column for this video mode
        self.text.cursor_row = cursor_loc / self.text.current_text_columns;
        self.text.cursor_col = cursor_loc % self.text.current_text_columns;
    }

    /// This function recalculates the line offset pixels (the row-pixels
    /// for the x86 video buffer). It forces a complete redraw if the
    /// line offset pixels value changes.
    ///
    /// Note that previously, this was called "line offset bytes", despite
    /// the fact that the value being set/calculated is actually in pixels.
    /// This was too confusing to keep.  Ideally, this routine *should*
    /// calculate the value in bytes, so that we don't have to keep multiplying
    /// by the pixel size every time we use it.  But at the present time,
    /// the risk seemed a little high, since we'd have to make sure we
    /// called this routine in *every* scenario where the mode might change
    /// and therefore the pixels-to-bytes calculation would need to be
    /// done again.
    fn calculate_line_offset_pixels(&mut self) {
        let line_offset = self.get_line_offset_pixels();
        if line_offset != self.state.persistent_state.line_offset_pixels {
            self.state.persistent_state.line_offset_pixels = line_offset;
            self.calculate_monitor_timing();
            self.force_screen_redraw();
        }
    }

    /// This routine recalculates the page start offset stored in four different CRT
    /// registers:
    ///
    /// bits 0-7    CRT reg 0x0C
    /// bits 8-15   CRT reg 0x0D
    ///
    /// if (bits 0-3 of CRT reg 0x69 are zero)
    ///   bits 16-17  CRT reg 0x31 bits 5-4
    ///   bits 18-19  CRT reg 0x51 bits 0-1
    /// else
    ///   bits 16-23  CRT reg 0x69 bits 0-7
    ///
    /// NOTE:  we have some confusion here.  CRT69 is defined as having the top 4
    /// bits reserved.  However, this seems to be some kind of mistake, because if
    /// that's so, there's no way to address all 4 meg of vram.  It makes logical
    /// sense to just use the whole register, and the comment above about bits 0-7
    /// seems to confirm that that's really what it's supposed to be.  I'm changing
    /// the bios to use CRT69 in this manner.  We'll still mask out the top two
    /// bits, to make sure we don't get an address beyond our hard-coded 4 meg vram.
    /// --richyam
    fn calculate_page_offset(&mut self) {
        let new_offset = self.get_page_offset();

        if self.state.persistent_state.cur_page_start_offset != new_offset {
            self.state.persistent_state.cur_page_start_offset = new_offset;

            if self.state.persistent_state.s3.enhanced_mode {
                self.force_screen_redraw();
            }
        }
    }

    /// This routine recalculates any S3 variables derived from
    /// other access variables. It should be called when any of
    /// the other values change.
    fn update_s3_state(&mut self) {
        // Update the graphics engine bits per pixel
        let pixel_len = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER]
            >> 4)
            & 0x3;

        match pixel_len {
            0 => {
                self.state.persistent_state.s3.bits_per_pixel = 8;
            }

            1 => {
                self.state.persistent_state.s3.bits_per_pixel = 16;
            }

            2 | 3 => {
                // Case 2 is undefined, but there is some
                // indicatation that it actually represents
                // 24-bpp mode. We don't support it.
                if pixel_len == 2 {
                    tracing::warn!("unexpected pixel length 2");
                }
                self.state.persistent_state.s3.bits_per_pixel = 32;
            }
            _ => unreachable!(),
        }

        // Determine the screen width used by graphics coprocessor
        let screen_width_code = ((self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER]
            >> 6)
            & 0x3)
            | ((self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_1_REGISTER]
                << 2)
                & 0x4);

        match screen_width_code {
            0 => {
                self.state.persistent_state.s3.screen_pixel_width = 1024;
            }

            1 => {
                self.state.persistent_state.s3.screen_pixel_width = 640;
            }

            2 => {
                self.state.persistent_state.s3.screen_pixel_width = 800;
            }

            3 => {
                self.state.persistent_state.s3.screen_pixel_width = 1280;
            }

            4 => {
                self.state.persistent_state.s3.screen_pixel_width = 1152;
            }

            5 => {
                // This is reserved on the real hardware, but used in the virtual hardware.
                self.state.persistent_state.s3.screen_pixel_width = 896;
            }

            6 => {
                self.state.persistent_state.s3.screen_pixel_width = 1600;
            }

            code => {
                // This is reserved on the real hardware, but unused in the virtual hardware.
                tracing::warn!(code, "unexpected screen width code");
            }
        }

        // Are we enabling/disabling enhanced mode?
        let enhanced_mode = (self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_MEMORY_CONFIGURATION_REGISTER]
            & 0x08)
            != 0;

        if self.state.persistent_state.s3.enhanced_mode != enhanced_mode {
            self.state.persistent_state.s3.enhanced_mode = enhanced_mode;

            self.update_access_vars();
            self.force_screen_redraw();
        }

        self.calculate_page_offset();
    }

    /// This routine recalculates the VGA access range. It should be
    /// called when the graphics controller misc register value is
    /// changed.
    fn calculate_vga_address_range(&mut self) {
        let range_start;
        let range_end;

        let address_mode = (self.state.persistent_state.vga_graphics_regs
            [VgaGraphicsReg::MEMORY_MAP_MODE_CONTROL_REGISTER]
            >> 2)
            & 0x3;

        // Calculate the new host memory staring/ending addresses
        match address_mode {
            0 => {
                range_start = 0xA0000;
                range_end = 0xC0000;
            }

            1 => {
                range_start = 0xA0000;
                range_end = 0xB0000;
            }

            2 => {
                range_start = 0xB0000;
                range_end = 0xB8000;
            }

            3 => {
                range_start = 0xB8000;
                range_end = 0xC0000;
            }
            _ => unreachable!(),
        }

        self.state.video_start_bus_range_offset = range_start;
        self.state.video_end_bus_range_offset = range_end;
    }

    fn write_pel_data_register(&mut self, data: u8) {
        let pel_reg_entry = self.state.persistent_state.pel_colors.as_mut_bytes();
        let len = pel_reg_entry.len();

        //
        // Only update the PEL register if the value changed
        //
        if pel_reg_entry[self.state.persistent_state.pel_reg_write_index as usize] != data {
            pel_reg_entry[self.state.persistent_state.pel_reg_write_index as usize] = data;

            //
            // Notify that the palette changed.
            //
            self.mark_palette_dirty();
        }

        self.state.persistent_state.pel_reg_write_index += 1;
        if self.state.persistent_state.pel_reg_write_index as usize >= len {
            self.state.persistent_state.pel_reg_write_index = 0;
        }
    }

    fn is_text_mode(&self) -> bool {
        self.state.persistent_state.video_enabled && self.state.persistent_state.text_mode
    }

    /// Considers modes like CGA, EGA, etc to be part of VGA mode.
    fn _is_vga_mode(&self) -> bool {
        self.state.persistent_state.video_enabled
            && !self.state.persistent_state.text_mode
            && !self.state.persistent_state.s3.enhanced_mode
    }

    fn _is_legacy_mode(&self) -> bool {
        self._is_vga_mode() || self.is_text_mode()
    }

    /// Requests that we become active video device.
    fn request_activation(&mut self) {
        if self.suppress_initial_activation {
            self.suppress_initial_activation = false;
        } else {
            todo!("ActivationRequested.Fire()");
        }
    }

    /// Records that a change has been made to the video mode,
    /// resolution or bit depth.
    fn record_video_mode_change(&mut self) {
        if self.suppress_initial_activation {
            // This is not the initial mode change, it is a fresh one.
            // Make sure it causes an activation when complete.
            self.suppress_initial_activation = false;
        }

        if !self.is_mode_change_pending {
            self.is_mode_change_pending = true;
            self.set_mode_change_timer();
        }
    }

    fn set_delayed_redraw_timer(&mut self) {
        self.is_delayed_redraw_timer_set = true;
        self.set_multi_purpose_timer();
    }

    fn set_multi_purpose_timer(&mut self) {
        // todo!()
    }

    fn set_mode_change_timer(&mut self) {
        // todo!()
    }

    fn evaluate_text_blink_timer_active_status(&mut self) {
        /*
        let textBlinkTimerShouldBeEnabled = self.IsTextMode()
            && self.text.BlinkingEnabled
            && self.IsRunning()
            && self.m_IsTheActiveVideo;

        if (textBlinkTimerShouldBeEnabled != (m_TextBlinkTimer->IsSet() == S_OK))
        {
            m_TextBlinkTimer->SetEnabled(textBlinkTimerShouldBeEnabled);
        }
        */
    }

    fn evaluate_cursor_blink_timer_active_status(&mut self) {
        /*        BOOL cursorBlinkTimerShouldBeEnabled =
            IsTextMode() &&
            m_TextModeState.DrawTextCursor &&
            IsRunning() &&
            m_IsTheActiveVideo;

        if (cursorBlinkTimerShouldBeEnabled != (m_CursorBlinkTimer->IsSet() == S_OK))
        {
            m_CursorBlinkTimer->SetEnabled(cursorBlinkTimerShouldBeEnabled);
        }*/
    }

    /// This function returns the proper mapping table for mapping
    /// an indexed PC palette into the direct colors.  It ensures that
    /// the tables are up to date for the current PC palette.
    fn rebuild_mapping_table(&mut self) {
        // If the palette has changed since the last time we generated the
        // mapping tables, we need to regenerate them.
        if self.palette_change_count == self.pixel_values_palette_count {
            return;
        }
        let max_index = if self.state.persistent_state.bits_per_pixel >= 8 {
            255
        } else {
            15
        };

        for pixel_value_index in 0..=max_index {
            self.mapping_table[pixel_value_index as usize] =
                self.vgacolor_index_to_argb32(pixel_value_index);
        }

        self.pixel_values_palette_count = self.palette_change_count;
    }

    fn update_render_state(&mut self) {
        self.rebuild_mapping_table();
        let render_state = if self.is_text_mode() {
            RenderState::Text(TextRenderState {
                text: self.text.clone(),
                cur_page_start_offset: self.state.persistent_state.cur_page_start_offset,
                mapping_table: self.mapping_table,
            })
        } else {
            RenderState::Graphics(GraphicsRenderState {
                bits_per_pixel: self.state.persistent_state.bits_per_pixel,
                cur_page_start_offset: self.state.persistent_state.cur_page_start_offset,
                line_offset_pixels: self.state.persistent_state.line_offset_pixels,
                width: self.state.persistent_state.pcvideo_width,
                height: self.state.persistent_state.adj_pcvideo_height,
                // Look at the dot clock to determine whether we need to pixel double
                pixel_double: self.state.persistent_state.vga_sequencer_regs
                    [VgaSequencerReg::CLOCKING_MODE_REGISTER]
                    & 0x08
                    != 0,
                pixel_pan: self.state.persistent_state.vga_attrib_regs
                    [VgaAttribReg::HORIZONTAL_PIXEL_PANNING_REGISTER]
                    & 7,
                mapping_table: self.mapping_table,
            })
        };
        self.render_control.update(render_state);
    }

    /// This function converts a color index into an RGB value. The
    /// method of conversion depends on the current mode.
    fn vgacolor_index_to_argb32(&self, index: u8) -> u32 {
        let color_reg_index = if (self.state.persistent_state.bits_per_pixel < 8)
            && (!self.state.persistent_state.enhanced_dac_mode)
        {
            // Turn off some of the planes if specified by the plane enable reg
            let palette_reg_index = index
                & self.state.persistent_state.vga_attrib_regs
                    [VgaAttribReg::COLOR_PLANE_ENABLE_REGISTER];

            let palette_reg_value =
                self.state.persistent_state.vga_attrib_regs[VgaAttribReg(palette_reg_index & 0xF)];

            if (self.state.persistent_state.vga_attrib_regs[VgaAttribReg::MODE_CONTROL_REGISTER]
                & spec::ATTRIBUTE_CONTROLLER_MODE_CONTROL_IPS_MASK)
                == 0
            {
                // Use bits 0-5 from palette register and bits 6-7 from pixel padding reg
                (palette_reg_value & 0x3F)
                    | ((self.state.persistent_state.vga_attrib_regs
                        [VgaAttribReg::PIXEL_PADDING_REGISTER]
                        << 4)
                        & 0xC0)
            } else {
                // Use bits 0-3 from palette register and bits 4-7 from pixel padding reg
                (palette_reg_value & 0x0F)
                    | ((self.state.persistent_state.vga_attrib_regs
                        [VgaAttribReg::PIXEL_PADDING_REGISTER]
                        << 4)
                        & 0xF0)
            }
        } else {
            index
        };

        let color_comp =
            self.state.persistent_state.pel_colors[color_reg_index as usize].red & 0x3F;
        let red = ((color_comp << 2) | (color_comp >> 4)) as u32;

        let color_comp =
            self.state.persistent_state.pel_colors[color_reg_index as usize].green & 0x3F;
        let green = ((color_comp << 2) | (color_comp >> 4)) as u32;

        let color_comp =
            self.state.persistent_state.pel_colors[color_reg_index as usize].blue & 0x3F;
        let blue = ((color_comp << 2) | (color_comp >> 4)) as u32;

        0xFF000000 | (red << 16) | (green << 8) | blue
    }

    fn mark_palette_dirty(&mut self) {
        self.palette_change_count += 1;
        self.force_screen_redraw();
    }

    /// Forces a screen redraw by arming the appropriate timer
    fn force_screen_redraw(&mut self) {
        //
        // A mode change trumps a full refresh, so if a mode change
        // is already pending, don't bother with a full refresh since
        // the mode change already includes a full refresh also.
        //
        if !self.is_mode_change_pending && !self.is_full_refresh_pending {
            self.is_full_refresh_pending = true;
            self.set_delayed_redraw_timer();
        }
    }

    fn get_line_offset_pixels(&self) -> u16 {
        // The value in the register(s) is innately half-the-desired-size, so we must double each piece,
        // either by multiplying by 2 or overshifting.

        let mut line_offset =
            self.state.persistent_state.crt_control_regs[CrtControlReg::OFFSET_REGISTER] as u16 * 2;

        if (self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER]
            & 0x30)
            != 0
        {
            line_offset += (self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER]
                as u16
                & 0x30)
                << 5;
        } else if (self.state.persistent_state.crt_control_regs
            [CrtControlReg::S3_EXTENDED_MODE_REGISTER]
            & 0x4)
            != 0
        {
            line_offset += 1 << 9;
        }

        // This is undocumented, but it appears that a zero line offset is equivalent
        // to 512. MAME (DOS) relies on this.
        if line_offset == 0 {
            line_offset = 512;
        }

        line_offset
    }

    /// Calculates the line compare value.
    ///
    /// If the computation logic changes, we need to update VerifyPersistentState() accordingly.
    fn get_line_compare_value(&self) -> u16 {
        // Calculate the line compare value stored in four places. We need to
        // OR these together to get the final value.
        self.state.persistent_state.crt_control_regs[CrtControlReg::LINE_COMPARE_REGISTER] as u16
            | ((self.state.persistent_state.crt_control_regs[CrtControlReg::OVERFLOW_REGISTER]
                as u16
                & 0x10)
                << 4)
            | ((self.state.persistent_state.crt_control_regs[CrtControlReg::MAX_SCANLINE_REGISTER]
                as u16
                & 0x40)
                << 3)
            | ((self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_EXTENDED_VERTICAL_OVERFLOW_REGISTER] as u16
                & 0x40)
                << 4)
    }

    fn get_page_offset(&self) -> u32 {
        let mut new_offset = self.state.persistent_state.crt_control_regs
            [CrtControlReg::START_ADDRESS_LO_REGISTER] as u32;

        new_offset |= (self.state.persistent_state.crt_control_regs
            [CrtControlReg::START_ADDRESS_HI_REGISTER] as u32)
            << 8;

        if self.state.persistent_state.s3.enhanced_mode {
            if self.state.persistent_state.crt_control_regs
                [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER]
                == 0
            {
                new_offset |= ((self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_MEMORY_CONFIGURATION_REGISTER]
                    as u32)
                    << (16 - 4))
                    & 0x00030000;

                new_offset |= ((self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_2_REGISTER]
                    as u32)
                    << 18)
                    & 0x000C0000;
            } else {
                new_offset |= (self.state.persistent_state.crt_control_regs
                    [CrtControlReg::S3_EXTENDED_SYSTEM_CONTROL_3_REGISTER]
                    as u32)
                    << 16;
            }
        }

        new_offset
    }

    fn displaying_splash_screen(&self) -> bool {
        self.state.persistent_state.crt_control_regs[CrtControlReg::CUSTOM_VS_BIOS_LOGO_REGISTER]
            != 0xFF
    }

    fn get_current_video_depth(&self) -> u8 {
        if self.displaying_splash_screen() {
            SPLASH_SCREEN_BYTES_PER_PIXEL * 8
        } else if self.state.persistent_state.text_mode {
            0
        } else {
            self.state.persistent_state.bits_per_pixel
        }
    }

    fn _get_current_video_width(&self) -> u16 {
        if self.displaying_splash_screen() {
            _SPLASH_SCREEN_WIDTH
        } else if self.state.persistent_state.text_mode {
            spec::TOTAL_VGA_HIRES_TEXT_COLUMNS as u16 * spec::VGA_HIRES_CHARACTER_WIDTH as u16
        } else {
            self.state.persistent_state.pcvideo_width
        }
    }

    fn _get_current_video_height(&self) -> u16 {
        if self.displaying_splash_screen() {
            _SPLASH_SCREEN_HEIGHT
        } else if self.state.persistent_state.text_mode {
            self.text.text_rows as u16 * self.text.text_char_height as u16
        } else {
            self.state.persistent_state.adj_pcvideo_height
        }
    }
}
