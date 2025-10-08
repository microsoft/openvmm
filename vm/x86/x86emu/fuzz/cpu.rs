// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arbitrary::Arbitrary;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use x86emu::Cpu;
use x86emu::RegisterIndex;
use x86emu::Segment;

#[derive(Debug, Arbitrary)]
pub(crate) struct FuzzerCpu {
    mem_data: [u8; 8],
    io_data: [u8; 4],
    xmm_val: u128,
    state: CpuState,
}

#[derive(Debug, Arbitrary)]
struct CpuState {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    pub gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    pub segs: [SegmentRegister; 6],
    pub rip: u64,
    pub rflags: RFlags,

    pub cr0: u64,
    pub efer: u64,
}

impl Cpu for FuzzerCpu {
    type Error = NeverError;

    async fn read_memory(
        &mut self,
        _gva: u64,
        bytes: &mut [u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        for c in bytes.chunks_mut(self.mem_data.len()) {
            c.copy_from_slice(&self.mem_data[..c.len()]);
        }
        Ok(())
    }

    async fn write_memory(
        &mut self,
        _gva: u64,
        _bytes: &[u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn compare_and_write_memory(
        &mut self,
        _gva: u64,
        _current: &[u8],
        _new: &[u8],
        _is_user_mode: bool,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn read_io(&mut self, _io_port: u16, bytes: &mut [u8]) -> Result<(), Self::Error> {
        bytes.copy_from_slice(&self.io_data[..bytes.len()]);
        Ok(())
    }

    async fn write_io(&mut self, _io_port: u16, _bytes: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn gp(&mut self, reg: RegisterIndex) -> u64 {
        self.state.gps[reg.extended_index as usize]
    }

    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64 {
        self.state.gps[reg.extended_index as usize] as i64
    }

    fn set_gp(&mut self, reg: RegisterIndex, v: u64) {
        self.state.gps[reg.extended_index as usize] = v;
    }

    fn rip(&mut self) -> u64 {
        self.state.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.state.rip = v;
    }

    fn segment(&mut self, index: Segment) -> SegmentRegister {
        self.state.segs[index as usize]
    }

    fn efer(&mut self) -> u64 {
        self.state.efer
    }

    fn cr0(&mut self) -> u64 {
        self.state.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.state.rflags
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.state.rflags = v;
    }

    /// Gets the value of an XMM* register.
    fn xmm(&mut self, _reg: usize) -> u128 {
        self.xmm_val
    }

    /// Sets the value of an XMM* register.
    fn set_xmm(&mut self, _reg: usize, _value: u128) {}
}

#[derive(Debug)]
pub enum NeverError {}
