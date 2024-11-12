// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZeroUsize;
use gdbstub::arch::RegId;

#[derive(Debug, Clone, Copy)]
pub enum X87FpuInternalRegId {
    Fctrl,
    Fstat,
    Ftag,
    Fiseg,
    Fioff,
    Foseg,
    Fooff,
    Fop,
}

impl X87FpuInternalRegId {
    fn from_u8(val: u8) -> Option<Self> {
        use self::X87FpuInternalRegId::*;

        let r = match val {
            0 => Fctrl,
            1 => Fstat,
            2 => Ftag,
            3 => Fiseg,
            4 => Fioff,
            5 => Foseg,
            6 => Fooff,
            7 => Fop,
            _ => return None,
        };
        Some(r)
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
pub enum X86SegmentRegId {
    CS,
    SS,
    DS,
    ES,
    FS,
    GS,
}

impl X86SegmentRegId {
    fn from_u8(val: u8) -> Option<Self> {
        use self::X86SegmentRegId::*;

        let r = match val {
            0 => CS,
            1 => SS,
            2 => DS,
            3 => ES,
            4 => FS,
            5 => GS,
            _ => return None,
        };
        Some(r)
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum X86_64CoreRegId {
    /// RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, r8-r15
    Gpr(u8),
    Rip,
    Eflags,
    Segment(X86SegmentRegId),
    FsBase,
    GsBase,
    KernelGsBase,
    Cr0,
    Cr2,
    Cr3,
    Cr4,
    Cr8,
    Efer,
    St(u8),
    Fpu(X87FpuInternalRegId),
    Xmm(u8),
    Mxcsr,
}

impl RegId for X86_64CoreRegId {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        use self::X86_64CoreRegId::*;

        let (r, sz): (X86_64CoreRegId, usize) = match id {
            0..=15 => (Gpr(id as u8), 8),
            16 => (Rip, 8),
            17 => (Eflags, 4),
            18..=23 => (Segment(X86SegmentRegId::from_u8(id as u8 - 18)?), 4),

            24 => (FsBase, 8),
            25 => (GsBase, 8),
            26 => (KernelGsBase, 8),
            27 => (Cr0, 8),
            28 => (Cr2, 8),
            29 => (Cr3, 8),
            30 => (Cr4, 8),
            31 => (Cr8, 8),
            32 => (Efer, 8),

            33..=40 => (St(id as u8 - 33), 10),
            41..=48 => (Fpu(X87FpuInternalRegId::from_u8(id as u8 - 41)?), 4),
            49..=64 => (Xmm(id as u8 - 49), 16),
            65 => (Mxcsr, 4),
            _ => return None,
        };

        Some((r, Some(NonZeroUsize::new(sz)?)))
    }
}
