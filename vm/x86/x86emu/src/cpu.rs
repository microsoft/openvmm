// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait for asynchronous callouts from the emulator to the VM.

use std::future::Future;

/// Trait for asynchronous callouts from the emulator to the VM.
pub trait Cpu {
    /// The error type for IO access failures.
    type Error;

    /// Performs a memory read of 1, 2, 4, or 8 bytes.
    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory write of 1, 2, 4, or 8 bytes.
    fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs an atomic, sequentially-consistent compare exchange on a memory
    /// location.
    ///
    /// The caller has already fetched `current` via `read_memory`, so the
    /// implementor only needs to perform an atomic compare+write if the memory
    /// could have mutated concurrently and supports atomic operation. This
    /// includes ordinary RAM, but does not include device registers.
    ///
    /// Returns `true` if the exchange succeeded, `false` otherwise.
    fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<bool, Self::Error>>;

    /// Performs an io read of 1, 2, or 4 bytes.
    fn read_io(
        &mut self,
        io_port: u16,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;
    /// Performs an io write of 1, 2, or 4 bytes.
    fn write_io(
        &mut self,
        io_port: u16,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Gets the value of an XMM* register.
    fn get_xmm(&mut self, reg: usize) -> Result<u128, Self::Error>;
    /// Sets the value of an XMM* register.
    fn set_xmm(&mut self, reg: usize, value: u128) -> Result<(), Self::Error>;
}

impl<T: Cpu + ?Sized> Cpu for &mut T {
    type Error = T::Error;

    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_memory(gva, bytes, is_user_mode)
    }

    fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).write_memory(gva, bytes, is_user_mode)
    }

    fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        is_user_mode: bool,
    ) -> impl Future<Output = Result<bool, Self::Error>> {
        (*self).compare_and_write_memory(gva, current, new, is_user_mode)
    }

    fn read_io(
        &mut self,
        io_port: u16,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_io(io_port, bytes)
    }

    fn write_io(
        &mut self,
        io_port: u16,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).write_io(io_port, bytes)
    }

    fn get_xmm(&mut self, reg: usize) -> Result<u128, Self::Error> {
        (*self).get_xmm(reg)
    }

    fn set_xmm(&mut self, reg: usize, value: u128) -> Result<(), Self::Error> {
        (*self).set_xmm(reg, value)
    }
}
