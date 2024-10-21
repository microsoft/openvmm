// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structs to hold register state for the x86 instruction emulator.

use iced_x86::Register;
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

impl CpuState {
    fn get_gp64(&self, reg: Register) -> u64 {
        debug_assert!(reg.is_gpr64());
        self.gps[reg.number()]
    }

    fn get_gp32(&self, reg: Register) -> u32 {
        debug_assert!(reg.is_gpr32());
        self.gps[reg.number()] as u32
    }

    fn get_gp16(&self, reg: Register) -> u16 {
        debug_assert!(reg.is_gpr16());
        self.gps[reg.number()] as u16
    }

    fn get_gp8(&self, reg: Register) -> u8 {
        debug_assert!(reg.is_gpr8());
        if reg >= Register::SPL {
            self.gps[reg.number() - 4] as u8
        } else if reg < Register::AH {
            self.gps[reg.number()] as u8
        } else {
            (self.gps[reg.number() - 4] >> 8) as u8
        }
    }

    fn set_gp64(&mut self, reg: Register, val: u64) {
        debug_assert!(reg.is_gpr64());
        self.gps[reg.number()] = val;
    }

    fn set_gp32(&mut self, reg: Register, val: u32) {
        debug_assert!(reg.is_gpr32());
        // N.B. setting a 32-bit register zero extends the result to the 64-bit
        //      register. This is different from 16-bit and 8-bit registers.
        self.gps[reg.number()] = val as u64;
    }

    fn set_gp16(&mut self, reg: Register, val: u16) {
        debug_assert!(reg.is_gpr16());
        self.gps[reg.number()] &= !0xffff;
        self.gps[reg.number()] |= val as u64;
    }

    fn set_gp8(&mut self, reg: Register, val: u8) {
        debug_assert!(reg.is_gpr8());
        if reg >= Register::SPL {
            self.gps[reg.number() - 4] &= !0xff;
            self.gps[reg.number() - 4] |= val as u64;
        } else if reg < Register::AH {
            self.gps[reg.number()] &= !0xff;
            self.gps[reg.number()] |= val as u64;
        } else {
            self.gps[reg.number() - 4] &= !0xff00;
            self.gps[reg.number() - 4] |= (val as u64) << 8;
        }
    }

    pub(crate) fn get_gp(&self, reg: Register) -> u64 {
        if reg.is_gpr64() {
            self.get_gp64(reg)
        } else if reg.is_gpr32() {
            self.get_gp32(reg).into()
        } else if reg.is_gpr16() {
            self.get_gp16(reg).into()
        } else {
            debug_assert!(reg.is_gpr8());
            self.get_gp8(reg).into()
        }
    }

    pub(crate) fn get_gp_sign_extend(&self, reg: Register) -> i64 {
        if reg.is_gpr64() {
            self.get_gp64(reg) as i64
        } else if reg.is_gpr32() {
            (self.get_gp32(reg) as i32).into()
        } else if reg.is_gpr16() {
            (self.get_gp16(reg) as i16).into()
        } else {
            debug_assert!(reg.is_gpr8());
            (self.get_gp8(reg) as i8).into()
        }
    }

    pub(crate) fn set_gp(&mut self, reg: Register, val: u64) {
        if reg.is_gpr64() {
            self.set_gp64(reg, val)
        } else if reg.is_gpr32() {
            self.set_gp32(reg, val as u32)
        } else if reg.is_gpr16() {
            self.set_gp16(reg, val as u16)
        } else {
            debug_assert!(reg.is_gpr8());
            self.set_gp8(reg, val as u8)
        }
    }

    pub(crate) fn bitness(&self) -> Bitness {
        if self.cr0 & x86defs::X64_CR0_PE != 0 {
            if self.efer & x86defs::X64_EFER_LMA != 0 {
                if self.segs[CpuState::CS].attributes.long() {
                    Bitness::Bit64
                } else {
                    Bitness::Bit32
                }
            } else {
                if self.segs[CpuState::CS].attributes.default() {
                    Bitness::Bit32
                } else {
                    Bitness::Bit16
                }
            }
        } else {
            Bitness::Bit16
        }
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
