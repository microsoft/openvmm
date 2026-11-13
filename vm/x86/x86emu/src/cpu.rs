// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait for asynchronous callouts from the emulator to the VM.

use crate::registers::RegisterIndex;
use crate::registers::Segment;
use std::future::Future;
use x86defs::RFlags;
use x86defs::SegmentRegister;

/// Trait for asynchronous callouts from the emulator to the VM.
///
/// Memory addresses already include the segment base.
pub trait Cpu {
    /// The error type for IO access failures.
    type Error;

    /// Performs a memory read.
    ///
    /// When `is_rmw` is set, this is the read of a read-modify-write
    /// operation and must validate write permissions for the entire range.
    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        is_user_mode: bool,
        is_rmw: bool,
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory write.
    ///
    /// Validate translation and permissions for the entire range before writing
    /// any bytes, so a fault on a later page leaves the store untouched.
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
    /// As with `write_memory`, a translation or permission fault on any page
    /// must leave memory untouched.
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

    fn gp(&mut self, reg: RegisterIndex) -> u64;
    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64;
    fn set_gp(&mut self, reg: RegisterIndex, v: u64);
    fn xmm(&mut self, index: usize) -> u128;
    fn set_xmm(&mut self, index: usize, v: u128);
    fn rip(&mut self) -> u64;
    fn set_rip(&mut self, v: u64);
    fn segment(&mut self, index: Segment) -> SegmentRegister;
    fn efer(&mut self) -> u64;
    fn cr0(&mut self) -> u64;
    fn cr4(&mut self) -> u64;
    fn rflags(&mut self) -> RFlags;
    fn set_rflags(&mut self, v: RFlags);
}

impl<T: Cpu + ?Sized> Cpu for &mut T {
    type Error = T::Error;

    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        is_user_mode: bool,
        is_rmw: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_memory(gva, bytes, is_user_mode, is_rmw)
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

    fn gp(&mut self, reg: RegisterIndex) -> u64 {
        (*self).gp(reg)
    }

    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64 {
        (*self).gp_sign_extend(reg)
    }

    fn set_gp(&mut self, reg: RegisterIndex, v: u64) {
        (*self).set_gp(reg, v)
    }

    fn xmm(&mut self, index: usize) -> u128 {
        (*self).xmm(index)
    }

    fn set_xmm(&mut self, index: usize, v: u128) {
        (*self).set_xmm(index, v)
    }

    fn rip(&mut self) -> u64 {
        (*self).rip()
    }

    fn set_rip(&mut self, v: u64) {
        (*self).set_rip(v);
    }

    fn segment(&mut self, index: Segment) -> SegmentRegister {
        (*self).segment(index)
    }

    fn efer(&mut self) -> u64 {
        (*self).efer()
    }

    fn cr0(&mut self) -> u64 {
        (*self).cr0()
    }

    fn cr4(&mut self) -> u64 {
        (*self).cr4()
    }

    fn rflags(&mut self) -> RFlags {
        (*self).rflags()
    }

    fn set_rflags(&mut self, v: RFlags) {
        (*self).set_rflags(v);
    }
}
