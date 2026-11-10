// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VpHaltReason;
use std::future::Future;
use vm_topology::processor::VpIndex;

/// This trait provides the operations between the VP dispatch loop and the
/// platform's devices.
pub trait CpuIo {
    /// Check if a given address will be handled by a device.
    fn is_mmio(&self, address: u64) -> bool;

    /// Gets the vector of the next interrupt to inject from the legacy
    /// interrupt controller (PIC) and sets the IRQ in service.
    fn acknowledge_pic_interrupt(&self) -> Option<u8>;

    /// Handle End Of Interrupt (EOI)
    ///
    /// A `u32` is used for the IRQ value for (future) ARM compat.
    fn handle_eoi(&self, irq: u32);

    /// Memory mapped IO read.
    #[must_use]
    fn read_mmio(&self, vp: VpIndex, address: u64, data: &mut [u8]) -> impl Future<Output = ()>;

    /// Memory mapped IO write.
    #[must_use]
    fn write_mmio(&self, vp: VpIndex, address: u64, data: &[u8]) -> impl Future<Output = ()>;

    /// Atomically compare and exchange MMIO when supported by the device.
    ///
    /// The default preserves legacy device-MMIO behavior: write `new` without
    /// comparing and report success.
    fn compare_exchange_mmio(
        &self,
        vp: VpIndex,
        address: u64,
        _current: &[u8],
        new: &[u8],
    ) -> impl Future<Output = std::io::Result<bool>> {
        async move {
            self.write_mmio(vp, address, new).await;
            Ok(true)
        }
    }

    /// Programmed IO read.
    #[must_use]
    fn read_io(&self, vp: VpIndex, port: u16, data: &mut [u8]) -> impl Future<Output = ()>;

    /// Programmed IO write.
    #[must_use]
    fn write_io(&self, vp: VpIndex, port: u16, data: &[u8]) -> impl Future<Output = ()>;

    /// Report an internal fatal error.
    ///
    /// The intention behind this method is to allow the top-level VMM
    /// to specify an error handling policy, while still being able to capture
    /// stacks and other context from the point of failure. We previously would
    /// return a `VpHaltReason::Panic` here, but that meant the stack trace
    /// would be from the point of the panic handler, which lost context.
    /// See vmotherboard's `FatalErrorPolicy` for an example.
    #[track_caller]
    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> VpHaltReason;
}
