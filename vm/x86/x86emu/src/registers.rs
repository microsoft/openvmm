// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structs to hold register state for the x86 instruction emulator.

use x86defs::RFlags;
use x86defs::SegmentRegister;

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

impl CpuState {
    /// Index of RAX in `gps`.
    pub const RAX: usize = 0;
    /// Index of RCX in `gps`.
    pub const RCX: usize = 1;
    /// Index of RDX in `gps`.
    pub const RDX: usize = 2;
    /// Index of RBX in `gps`.
    pub const RBX: usize = 3;
    /// Index of RSP in `gps`.
    pub const RSP: usize = 4;
    /// Index of RBP in `gps`.
    pub const RBP: usize = 5;
    /// Index of RSI in `gps`.
    pub const RSI: usize = 6;
    /// Index of RDI in `gps`.
    pub const RDI: usize = 7;
    /// Index of R8 in `gps`.
    pub const R8: usize = 8;
    /// Index of R9 in `gps`.
    pub const R9: usize = 9;
    /// Index of R10 in `gps`.
    pub const R10: usize = 10;
    /// Index of R11 in `gps`.
    pub const R11: usize = 11;
    /// Index of R12 in `gps`.
    pub const R12: usize = 12;
    /// Index of R13 in `gps`.
    pub const R13: usize = 13;
    /// Index of R14 in `gps`.
    pub const R14: usize = 14;
    /// Index of R15 in `gps`.
    pub const R15: usize = 15;

    /// Index of ES in `segs`.
    pub const ES: usize = 0;
    /// Index of CS in `segs`.
    pub const CS: usize = 1;
    /// Index of SS in `segs`.
    pub const SS: usize = 2;
    /// Index of DS in `segs`.
    pub const DS: usize = 3;
    /// Index of FS in `segs`.
    pub const FS: usize = 4;
    /// Index of GS in `segs`.
    pub const GS: usize = 5;
}

pub fn bitness(cr0: u64, efer: u64, cs: SegmentRegister) -> Bitness {
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
pub enum Bitness {
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
