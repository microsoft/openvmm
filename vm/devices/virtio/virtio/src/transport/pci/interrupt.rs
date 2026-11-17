// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The interrupt outputs of the PCI transport across a restore.
//!
//! A restore holds the INTx line until the device starts: it restores the
//! interrupt status without driving the line, since the interrupt controller
//! that the line reaches may itself still be restoring, and the start then
//! drives the line to the restored status. A restore sends no MSI-X message:
//! the queues' messages begin with the queues, which start only with the
//! device, and the vectors keep the messages that they held pending at the
//! save in their pending bits.

use super::InterruptKind;
use super::PciTransport;

impl PciTransport {
    /// Restores the interrupt status and the MSI-X configuration vector, and
    /// holds the INTx line until [`Self::release_held_interrupts`].
    pub(super) fn restore_interrupts(&mut self, status: u32, msix_config_vector: u16) {
        *self.interrupt_status.lock() = status;
        self.msix_config_vector = msix_config_vector;
        self.held = matches!(self.interrupt_kind, InterruptKind::IntX(_));
    }

    /// Ends the hold of a restore, as the device starts: drives the INTx line
    /// to the restored status.
    pub(super) fn release_held_interrupts(&mut self) {
        if std::mem::take(&mut self.held)
            && let InterruptKind::IntX(line) = &self.interrupt_kind
        {
            line.set_level(*self.interrupt_status.lock() != 0);
        }
    }
}
