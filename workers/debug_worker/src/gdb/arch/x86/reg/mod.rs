// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Register` structs for x86 architectures.

use gdbstub::arch::Registers;

/// `RegId` definitions for x86 architectures.
#[allow(dead_code)]
pub mod id;

mod core32;
mod core64;

pub use core32::X86CoreRegs;
pub use core64::X86_64CoreRegs;

/// 80-bit floating point value
pub type F80 = [u8; 10];

/// FPU registers
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct X87FpuInternalRegs {
    /// Floating-point control register
    pub fctrl: u32,
    /// Floating-point status register
    pub fstat: u32,
    /// Tag word
    pub ftag: u32,
    /// FPU instruction pointer segment
    pub fiseg: u32,
    /// FPU instruction pointer offset
    pub fioff: u32,
    /// FPU operand segment
    pub foseg: u32,
    /// FPU operand offset
    pub fooff: u32,
    /// Floating-point opcode
    pub fop: u32,
}

impl Registers for X87FpuInternalRegs {
    type ProgramCounter = u32;

    // HACK: this struct is never used as an architecture's main register file, so
    // using a dummy value here is fine.
    fn pc(&self) -> Self::ProgramCounter {
        0
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        macro_rules! write_bytes {
            ($bytes:expr) => {
                for b in $bytes {
                    write_byte(Some(*b))
                }
            };
        }

        // Note: GDB section names don't make sense unless you read x87 FPU section 8.1:
        // https://web.archive.org/web/20150123212110/http://www.intel.com/content/dam/www/public/us/en/documents/manuals/64-ia-32-architectures-software-developer-vol-1-manual.pdf
        write_bytes!(&self.fctrl.to_le_bytes());
        write_bytes!(&self.fstat.to_le_bytes());
        write_bytes!(&self.ftag.to_le_bytes());
        write_bytes!(&self.fiseg.to_le_bytes());
        write_bytes!(&self.fioff.to_le_bytes());
        write_bytes!(&self.foseg.to_le_bytes());
        write_bytes!(&self.fooff.to_le_bytes());
        write_bytes!(&self.fop.to_le_bytes());
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() != 0x20 {
            return Err(());
        }

        let mut regs = bytes
            .chunks_exact(4)
            .map(|x| u32::from_le_bytes(x.try_into().unwrap()));

        self.fctrl = regs.next().ok_or(())?;
        self.fstat = regs.next().ok_or(())?;
        self.ftag = regs.next().ok_or(())?;
        self.fiseg = regs.next().ok_or(())?;
        self.fioff = regs.next().ok_or(())?;
        self.foseg = regs.next().ok_or(())?;
        self.fooff = regs.next().ok_or(())?;
        self.fop = regs.next().ok_or(())?;

        Ok(())
    }
}

/// x86 segment registers.
///
/// Source: <https://github.com/bminor/binutils-gdb/blob/master/gdb/features/i386/64bit-core.xml>
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct X86SegmentRegs {
    /// Code Segment
    pub cs: u32,
    /// Stack Segment
    pub ss: u32,
    /// Data Segment
    pub ds: u32,
    /// Extra Segment
    pub es: u32,
    /// General Purpose Segment
    pub fs: u32,
    /// General Purpose Segment
    pub gs: u32,
}

impl Registers for X86SegmentRegs {
    type ProgramCounter = u32;

    // HACK: this struct is never used as an architecture's main register file, so
    // using a dummy value here is fine.
    fn pc(&self) -> Self::ProgramCounter {
        0
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        macro_rules! write_bytes {
            ($bytes:expr) => {
                for b in $bytes {
                    write_byte(Some(*b))
                }
            };
        }

        write_bytes!(&self.cs.to_le_bytes());
        write_bytes!(&self.ss.to_le_bytes());
        write_bytes!(&self.ds.to_le_bytes());
        write_bytes!(&self.es.to_le_bytes());
        write_bytes!(&self.fs.to_le_bytes());
        write_bytes!(&self.gs.to_le_bytes());
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() != size_of::<u32>() * 6 {
            return Err(());
        }

        let mut regs = bytes
            .chunks_exact(4)
            .map(|x| u32::from_le_bytes(x.try_into().unwrap()));

        self.cs = regs.next().ok_or(())?;
        self.ss = regs.next().ok_or(())?;
        self.ds = regs.next().ok_or(())?;
        self.es = regs.next().ok_or(())?;
        self.fs = regs.next().ok_or(())?;
        self.gs = regs.next().ok_or(())?;

        Ok(())
    }
}
