// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel 8042 definitions.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

#[derive(Inspect)]
#[bitfield(u8)]
pub struct OutputPort {
    pub reset: bool,
    pub a20_gate: bool,
    pub aux_clock: bool,
    pub aux_data: bool,
    pub keyboard_output_buffered: bool,
    pub mouse_output_buffered: bool,
    pub clock: bool,
    pub data: bool,
}

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum ControllerCommand: u8 {
        READ_COMMAND_BYTE       = 0x20,
        WRITE_COMMAND_BYTE      = 0x60,
        UNKNOWN_A1              = 0xA1,     // Used in AMI BIOS boot
        PWD_CHECK               = 0xA4,
        DISABLE_AUX_INTERFACE   = 0xA7,
        ENABLE_AUX_INTERFACE    = 0xA8,
        CHECK_AUX_INTERFACE     = 0xA9,
        SELF_TEST               = 0xAA,
        CHECK_INTERFACE         = 0xAB,
        DISABLE_KEYBOARD        = 0xAD,
        ENABLE_KEYBOARD         = 0xAE,
        READ_INPUT_PORT         = 0xC0,
        READ_OUT_INPUT_PORT_LO  = 0xC1,
        READ_OUT_INPUT_PORT_HI  = 0xC2,
        READ_OUTPUT_PORT        = 0xD0,
        WRITE_OUTPUT_PORT       = 0xD1,
        WRITE_OUTPUT_BUFFER     = 0xD2,
        WRITE_AUX_OUTPUT_BUFFER = 0xD3,
        WRITE_AUX_DEVICE        = 0xD4,
        PULSE_OUTPUT_F0         = 0xF0,
        PULSE_OUTPUT_F1         = 0xF1,
        PULSE_OUTPUT_F2         = 0xF2,
        PULSE_OUTPUT_F3         = 0xF3,
        PULSE_OUTPUT_F4         = 0xF4,
        PULSE_OUTPUT_F5         = 0xF5,
        PULSE_OUTPUT_F6         = 0xF6,
        PULSE_OUTPUT_F7         = 0xF7,
        PULSE_OUTPUT_F8         = 0xF8,
        PULSE_OUTPUT_F9         = 0xF9,
        PULSE_OUTPUT_FA         = 0xFA,
        PULSE_OUTPUT_FB         = 0xFB,
        PULSE_OUTPUT_FC         = 0xFC,
        PULSE_OUTPUT_FD         = 0xFD,
        PULSE_OUTPUT_FE         = 0xFE,
        PULSE_OUTPUT_FF         = 0xFF,
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct KeyboardStatus {
    pub output_buffer_full: bool,
    pub input_buffer_full: bool,
    pub keyboard_self_test: bool,
    pub input_buffer_for_controller: bool,
    pub keyboard_unlocked: bool,
    pub output_buffer_for_mouse: bool,
    pub timeout_error: bool,
    pub parity_error: bool,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct CommandFlag {
    pub allow_keyboard_interrupts: bool,
    pub allow_mouse_interrupts: bool,
    pub keyboard_self_test: bool,
    pub unused: bool,
    pub disable_keyboard: bool,
    pub disable_mouse: bool,
    pub enable_scan_code: bool,
    pub unused2: bool,
}
