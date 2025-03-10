// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use hvdef::Vtl;
use vm_topology::processor::VpIndex;

/// This trait provides the operations between the VP dispatch loop and the
/// platform's devices.
pub trait CpuIo: Send + Sync {
    /// Check if a given address will be handled by a device.
    fn is_mmio(&self, address: u64) -> bool;

    /// Gets the vector of the next interrupt to inject from the legacy
    /// interrupt controller (PIC) and sets the IRQ in service.
    fn acknowledge_pic_interrupt(&self) -> Option<u8>;

    /// Handle End Of Interrupt (EOI)
    ///
    /// A `u32` is used for the IRQ value for (future) ARM compat.
    fn handle_eoi(&self, irq: u32);

    /// Signal a synic event.
    fn signal_synic_event(&self, vtl: Vtl, connection_id: u32, flag: u16) -> hvdef::HvResult<()>;

    /// Post a synic message.
    fn post_synic_message(
        &self,
        vtl: Vtl,
        connection_id: u32,
        secure: bool,
        message: &[u8],
    ) -> hvdef::HvResult<()>;

    /// Memory mapped IO read.
    #[must_use]
    fn read_mmio(&self, vp: VpIndex, address: u64, data: &mut [u8]) -> impl Future<Output = ()>;

    /// Memory mapped IO write.
    #[must_use]
    fn write_mmio(&self, vp: VpIndex, address: u64, data: &[u8]) -> impl Future<Output = ()>;

    /// Programmed IO read.
    #[must_use]
    fn read_io(&self, vp: VpIndex, port: u16, data: &mut [u8]) -> impl Future<Output = ()>;

    /// Programmed IO write.
    #[must_use]
    fn write_io(&self, vp: VpIndex, port: u16, data: &[u8]) -> impl Future<Output = ()>;
}
