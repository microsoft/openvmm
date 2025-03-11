// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub trait Cpu: AccessCpuState {
    /// The error type for IO access failures.
    type Error;

    /// Performs a memory read of an instruction to execute.
    fn read_instruction(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory read of 1, 2, 4, or 8 bytes.
    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory read of 1, 2, 4, or 8 bytes on a guest physical address.
    fn read_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory write of 1, 2, 4, or 8 bytes.
    fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs a memory write of 1, 2, 4, or 8 bytes on a guest physical address.
    fn write_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Performs an atomic, sequentially-consistent compare exchange on a memory
    /// location.
    ///
    /// The caller has already fetched `current` via `read_memory`, so the
    /// implementor only needs to perform an atomic compare+write if the memory
    /// could have mutated concurrently and supports atomic operation. This
    /// includes ordinary RAM, but does not include device registers.
    ///
    /// Sets `*success` to `true` if the exchange succeeded, `false` otherwise.
    ///
    /// FUTURE: just return `success` when we can directly use async functions
    /// in traits.
    fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        success: &mut bool,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

impl<T: Cpu + ?Sized> Cpu for &mut T {
    type Error = T::Error;

    fn read_instruction(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_memory(gva, bytes)
    }

    fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_memory(gva, bytes)
    }

    fn read_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).read_physical_memory(gpa, bytes)
    }

    fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).write_memory(gva, bytes)
    }

    fn write_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).write_physical_memory(gpa, bytes)
    }

    fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        success: &mut bool,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        (*self).compare_and_write_memory(gva, current, new, success)
    }
}

pub trait AccessCpuState {
    /// Commit any outstanding register updates to the CPU.
    fn commit(&mut self);

    /// Access general purpose x register and index (e.g. X0).
    fn x(&mut self, index: u8) -> u64;

    /// Update general purpose x register at index with value (e.g. X0 = 1).
    fn update_x(&mut self, index: u8, data: u64);

    /// Access floating point 128-bit register at index (e.g. Q0).
    fn q(&self, index: u8) -> u128;

    /// Update floating point 128-bit register at index (e.g. Q0 = 1.0).
    fn update_q(&mut self, index: u8, data: u128);

    /// Access floating point 64-bit register at index (e.g. D0).
    fn d(&self, index: u8) -> u64;

    /// Update floating point 64-bit register at index (e.g. D0 = 1.0).
    fn update_d(&mut self, index: u8, data: u64);

    /// Access floating point 32-bit register at index (e.g. H0).
    fn h(&self, index: u8) -> u32;

    /// Update floating point 32-bit register at index (e.g. H0 = 1.0).
    fn update_h(&mut self, index: u8, data: u32);

    /// Access floating point 16-bit register at index (e.g. S0).
    fn s(&self, index: u8) -> u16;

    /// Update floating point 16-bit register at index (e.g. S0 = 1.0).
    fn update_s(&mut self, index: u8, data: u16);

    /// Access floating point 16-bit register at index (e.g. B0).
    fn b(&self, index: u8) -> u8;

    /// Update floating point 8-bit register at index (e.g. B0 = 1.0).
    fn update_b(&mut self, index: u8, data: u8);

    /// Access stack pointer register.
    fn sp(&mut self) -> u64;

    /// Update stack pointer register.
    fn update_sp(&mut self, data: u64);

    /// Access frame pointer register (alias for X29).
    fn fp(&mut self) -> u64;

    /// Update frame pointer register (alias for X29).
    fn update_fp(&mut self, data: u64);

    /// Access link register / return address (alias for X30).
    fn lr(&mut self) -> u64;

    /// Update link register / return address (alias for X30).
    fn update_lr(&mut self, data: u64);

    /// Access program counter register / instruction pointer.
    fn pc(&mut self) -> u64;

    /// Update program counter register / instruction pointer.
    fn update_pc(&mut self, data: u64);

    /// Access the CSPR register
    fn cpsr(&mut self) -> aarch64defs::Cpsr64;
}

impl<T: AccessCpuState + ?Sized> AccessCpuState for &mut T {
    fn commit(&mut self) {
        (*self).commit()
    }
    fn x(&mut self, index: u8) -> u64 {
        (*self).x(index)
    }
    fn update_x(&mut self, index: u8, data: u64) {
        (*self).update_x(index, data)
    }
    fn q(&self, index: u8) -> u128 {
        (**self).q(index)
    }
    fn update_q(&mut self, index: u8, data: u128) {
        (*self).update_q(index, data)
    }
    fn d(&self, index: u8) -> u64 {
        (**self).d(index)
    }
    fn update_d(&mut self, index: u8, data: u64) {
        (*self).update_d(index, data)
    }
    fn h(&self, index: u8) -> u32 {
        (**self).h(index)
    }
    fn update_h(&mut self, index: u8, data: u32) {
        (*self).update_h(index, data)
    }
    fn s(&self, index: u8) -> u16 {
        (**self).s(index)
    }
    fn update_s(&mut self, index: u8, data: u16) {
        (*self).update_s(index, data)
    }
    fn b(&self, index: u8) -> u8 {
        (**self).b(index)
    }
    fn update_b(&mut self, index: u8, data: u8) {
        (*self).update_b(index, data)
    }
    fn sp(&mut self) -> u64 {
        (*self).sp()
    }
    fn update_sp(&mut self, data: u64) {
        (*self).update_sp(data)
    }
    fn fp(&mut self) -> u64 {
        (*self).fp()
    }
    fn update_fp(&mut self, data: u64) {
        (*self).update_fp(data)
    }
    fn lr(&mut self) -> u64 {
        (*self).lr()
    }
    fn update_lr(&mut self, data: u64) {
        (*self).update_lr(data)
    }
    fn pc(&mut self) -> u64 {
        (*self).pc()
    }
    fn update_pc(&mut self, data: u64) {
        (*self).update_pc(data)
    }
    fn cpsr(&mut self) -> aarch64defs::Cpsr64 {
        (*self).cpsr()
    }
}
