// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::F80;
use super::X86SegmentRegs;
use super::X87FpuInternalRegs;
use gdbstub::arch::Registers;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct X86_64CoreRegs {
    /// RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, r8-r15
    pub regs: [u64; 16],
    pub rip: u64,
    pub eflags: u32,
    pub segments: X86SegmentRegs,

    pub fs_base: u64,
    pub gs_base: u64,
    pub k_gs_base: u64,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,

    pub st: [F80; 8],
    pub fpu: X87FpuInternalRegs,
    pub xmm: [u128; 0x10],
    pub mxcsr: u32,
}

impl Registers for X86_64CoreRegs {
    type ProgramCounter = u64;

    fn pc(&self) -> Self::ProgramCounter {
        self.rip
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        macro_rules! write_bytes {
            ($bytes:expr) => {
                for b in $bytes {
                    write_byte(Some(*b))
                }
            };
        }

        // rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp, r8-r15
        for reg in &self.regs {
            write_bytes!(&reg.to_le_bytes());
        }

        write_bytes!(&self.rip.to_le_bytes());
        write_bytes!(&self.eflags.to_le_bytes());

        self.segments.gdb_serialize(&mut write_byte);

        write_bytes!(&self.fs_base.to_le_bytes());
        write_bytes!(&self.gs_base.to_le_bytes());
        write_bytes!(&self.k_gs_base.to_le_bytes());

        write_bytes!(&self.cr0.to_le_bytes());
        write_bytes!(&self.cr2.to_le_bytes());
        write_bytes!(&self.cr3.to_le_bytes());
        write_bytes!(&self.cr4.to_le_bytes());
        write_bytes!(&self.cr8.to_le_bytes());
        write_bytes!(&self.efer.to_le_bytes());

        for st_reg in &self.st {
            write_bytes!(st_reg);
        }

        self.fpu.gdb_serialize(&mut write_byte);

        for xmm_reg in &self.xmm {
            write_bytes!(&xmm_reg.to_le_bytes());
        }

        write_bytes!(&self.mxcsr.to_le_bytes());
    }

    fn gdb_deserialize(&mut self, mut bytes: &[u8]) -> Result<(), ()> {
        let mut take_bytes = |n: usize| -> Result<&[u8], ()> {
            if bytes.len() < n {
                return Err(());
            }
            let (ret, remaining) = bytes.split_at(n);
            bytes = remaining;
            Ok(ret)
        };

        let regs = take_bytes(16 * 8)?
            .chunks_exact(8)
            .map(|x| u64::from_le_bytes(x.try_into().unwrap()));

        for (reg, src) in self.regs.iter_mut().zip(regs) {
            *reg = src;
        }

        self.rip = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.eflags = u32::from_le_bytes(take_bytes(4)?.try_into().unwrap());

        self.segments.gdb_deserialize(take_bytes(4 * 6)?)?;

        self.fs_base = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.gs_base = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.k_gs_base = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());

        self.cr0 = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.cr2 = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.cr3 = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.cr4 = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.cr8 = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());
        self.efer = u64::from_le_bytes(take_bytes(8)?.try_into().unwrap());

        let st = take_bytes(8 * 10)?
            .chunks_exact(10)
            .map(|x| <[u8; 10]>::try_from(x).unwrap());

        for (reg, src) in self.st.iter_mut().zip(st) {
            *reg = src;
        }

        self.fpu.gdb_deserialize(take_bytes(32)?)?;

        let xmm = take_bytes(16 * 16)?
            .chunks_exact(16)
            .map(|x| u128::from_le_bytes(x.try_into().unwrap()));

        for (reg, src) in self.xmm.iter_mut().zip(xmm) {
            *reg = src;
        }

        self.mxcsr = u32::from_le_bytes(take_bytes(4)?.try_into().unwrap());

        Ok(())
    }
}
