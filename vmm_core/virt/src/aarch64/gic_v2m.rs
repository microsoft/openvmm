// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GIC v2m MSI frame support for delivering PCIe MSIs as SPI assertions.

use crate::irqcon::ControlGic;
use aarch64defs::gic::GicV2mRegister;
use pci_core::msi::SignalMsi;
use std::ops::Range;
use std::sync::Arc;
use vm_topology::processor::aarch64::GicV2mInfo;

/// A [`SignalMsi`] implementation that decodes GIC v2m-style MSIs and delivers
/// them as SPI assertions via [`ControlGic`].
///
/// When a device fires an MSI it writes the assigned GIC interrupt ID to the
/// SETSPI_NS register inside the v2m frame (`frame_base + 0x0040`). The host
/// intercepts that write (or for software devices, synthesises it) and calls
/// [`signal_msi`](SignalMsi::signal_msi) with `address = frame_base + 0x0040`
/// and `data = interrupt_id`. This struct validates the address and SPI range
/// then calls [`ControlGic::set_spi_irq`].
pub struct GicV2mSignalMsi {
    /// Address of the v2m SETSPI_NS doorbell, i.e. `frame_base + 0x0040`.
    setspi_addr: u64,
    /// The SPI interrupt IDs owned by this v2m frame.
    spi_range: Range<u32>,
    irqcon: Arc<dyn ControlGic>,
}

impl GicV2mSignalMsi {
    /// Create a new `GicV2mSignalMsi` from v2m frame info and a GIC controller.
    pub fn new(v2m: &GicV2mInfo, irqcon: Arc<dyn ControlGic>) -> Self {
        Self {
            setspi_addr: v2m.frame_base + GicV2mRegister::SETSPI_NS.0 as u64,
            spi_range: v2m.spi_base..v2m.spi_base + v2m.spi_count,
            irqcon,
        }
    }
}

impl SignalMsi for GicV2mSignalMsi {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        if address != self.setspi_addr {
            tracelimit::warn_ratelimited!(
                address,
                data,
                "unexpected MSI address (expected v2m SETSPI_NS)"
            );
            return;
        }
        if !self.spi_range.contains(&data) {
            tracelimit::warn_ratelimited!(data, "MSI data (SPI ID) outside v2m SPI range");
            return;
        }
        self.irqcon.set_spi_irq(data, true);
    }
}
