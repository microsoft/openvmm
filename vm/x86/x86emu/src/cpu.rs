// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait for asynchronous callouts from the emulator to the VM.

use crate::{Cr0, Efer, Gp, Rip, Xmm};
use iced_x86::Register;
use x86defs::{RFlags, SegmentRegister};
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

    fn gp(&mut self, reg: Register) -> Gp;
    fn gp_sign_extend(&mut self, reg: Register) -> i64;
    fn set_gp(&mut self, reg: Register, v: Gp);
    fn xmm(&mut self, index: usize) -> Xmm;
    fn set_xmm(&mut self, index: usize, v: Xmm) -> Result<(), Self::Error>;
    fn rip(&mut self) -> Rip;
    fn set_rip(&mut self, v: Rip);
    fn segment(&mut self, index: usize) -> SegmentRegister;
    fn efer(&mut self) -> Efer;
    fn cr0(&mut self) -> Cr0;
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

    fn gp(&mut self, reg: Register) -> Gp {
        (*self).gp(reg)
    }

    fn gp_sign_extend(&mut self, reg: Register) -> i64 {
        (*self).gp_sign_extend(reg)
    }

    fn set_gp(&mut self, reg: Register, v: Gp) {
        (*self).set_gp(reg, v)
    }

    fn xmm(&mut self, index: usize) -> Xmm {
        (*self).xmm(index)
    }

    fn set_xmm(&mut self, index: usize, v: Xmm) -> Result<(), Self::Error> {
        (*self).set_xmm(index, v)
    }

    fn rip(&mut self) -> Rip {
        (*self).rip()
    }

    fn set_rip(&mut self, v: Rip) {
        (*self).set_rip(v);
    }

    fn segment(&mut self, index: usize) -> SegmentRegister {
        (*self).segment(index)
    }

    fn efer(&mut self) -> Efer {
        (*self).efer()
    }

    fn cr0(&mut self) -> Cr0 {
        (*self).cr0()
    }

    fn rflags(&mut self) -> RFlags {
        (*self).rflags()
    }

    fn set_rflags(&mut self, v: RFlags) {
        (*self).set_rflags(v);
    }
}
