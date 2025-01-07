// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structs to hold register state for the x86 instruction emulator.

use x86defs::RFlags;
use x86defs::SegmentRegister;

#[repr(usize)]
#[derive(Debug, Copy, Clone, PartialEq)]
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
pub enum GpSize {
    ///8-bit registers have a shift value, depending on if we're capturing the high/low bits
    BYTE(usize),
    WORD,
    DWORD,
    QWORD,
}

#[repr(usize)]
#[derive(Debug, Copy, Clone)]
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

//TODO(babayet2) this should be killed after each emulator implementation defines its own cache
/// The current CPU register state. Some of the fields are updated by the emulator.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct CpuState {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    pub gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    /// Immutable for now.
    pub segs: [SegmentRegister; 6],
    /// RIP.
    pub rip: u64,
    /// RFLAGS.
    pub rflags: RFlags,

    /// CR0. Immutable.
    pub cr0: u64,
    /// EFER. Immutable.
    pub efer: u64,
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
