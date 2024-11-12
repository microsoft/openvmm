// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arbitrary::Arbitrary;
use x86emu::Cpu;

#[derive(Debug, Arbitrary)]
pub(crate) struct FuzzerCpu {
    mem_data: [u8; 8],
    io_data: [u8; 4],
    xmm_val: u128,
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

    fn get_xmm(&mut self, _reg: usize) -> Result<u128, Self::Error> {
        Ok(self.xmm_val)
    }

    fn set_xmm(&mut self, _reg: usize, _value: u128) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum NeverError {}
