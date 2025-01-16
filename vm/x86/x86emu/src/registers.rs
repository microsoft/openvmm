// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structs to hold register state for the x86 instruction emulator.

use x86defs::SegmentRegister;

#[repr(usize)]
#[derive(Debug, Copy, Clone, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
pub enum Gp {
    RAX = 0,
    RCX = 1,
    RDX = 2,
    RBX = 3,
    RSP = 4,
    RBP = 5,
    RSI = 6,
    RDI = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

#[derive(Debug, Copy, Clone)]
#[allow(clippy::upper_case_acronyms)]
pub enum GpSize {
    /// 8-bit registers have a shift value, depending on if we're capturing the high/low bits
    BYTE(usize),
    WORD,
    DWORD,
    QWORD,
}

#[repr(usize)]
#[derive(Debug, Copy, Clone)]
#[allow(clippy::upper_case_acronyms)]
pub enum Segment {
    ES = 0,
    CS = 1,
    SS = 2,
    DS = 3,
    FS = 4,
    GS = 5,
}

#[derive(Debug, Copy, Clone)]
pub struct RegisterIndex {
    /// Index of the full register size. E.g. this would be the index of RAX for the register EAX.
    pub extended_index: Gp,
    /// The size of the register, including a shift for 8-bit registers
    pub size: GpSize,
}

impl RegisterIndex {
    /// Converts the internal emulator representation of a register into a u64
    /// e.g. for AL, this consumes the value of RAX and outputs the low 8 bits
    pub fn apply_sizing(&self, v: u64) -> u64 {
        match self.size {
            GpSize::BYTE(shift) => ((v >> shift) as u8).into(),
            GpSize::WORD => (v as u16).into(),
            GpSize::DWORD => (v as u32).into(),
            GpSize::QWORD => v,
        }
    }

    pub fn apply_sizing_signed(&self, v: u64) -> i64 {
        match self.size {
            GpSize::BYTE(shift) => ((v >> shift) as i8).into(),
            GpSize::WORD => (v as i16).into(),
            GpSize::DWORD => (v as i32).into(),
            GpSize::QWORD => v as i64,
        }
    }

    pub fn apply_update(&self, extended_register: u64, v: u64) -> u64 {
        match self.size {
            GpSize::BYTE(shift) => {
                let mask = !(0xff << shift);
                (extended_register & mask) | (((v as u8) as u64) << shift)
            }
            GpSize::WORD => (extended_register & !0xffff) | (v as u16) as u64,
            // N.B. setting a 32-bit register zero extends the result to the 64-bit
            //      register. This is different from 16-bit and 8-bit registers.
            GpSize::DWORD => (v as u32) as u64,
            GpSize::QWORD => v,
        }
    }
}

impl From<Gp> for RegisterIndex {
    fn from(val: Gp) -> Self {
        RegisterIndex {
            extended_index: val,
            size: GpSize::QWORD,
        }
    }
}

pub(crate) fn bitness(cr0: u64, efer: u64, cs: SegmentRegister) -> Bitness {
    if cr0 & x86defs::X64_CR0_PE != 0 {
        if efer & x86defs::X64_EFER_LMA != 0 {
            if cs.attributes.long() {
                Bitness::Bit64
            } else {
                Bitness::Bit32
            }
        } else {
            if cs.attributes.default() {
                Bitness::Bit32
            } else {
                Bitness::Bit16
            }
        }
    } else {
        Bitness::Bit16
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Bitness {
    Bit64,
    Bit32,
    Bit16,
}

impl From<Bitness> for u32 {
    fn from(bitness: Bitness) -> u32 {
        match bitness {
            Bitness::Bit64 => 64,
            Bitness::Bit32 => 32,
            Bitness::Bit16 => 16,
        }
    }
}
