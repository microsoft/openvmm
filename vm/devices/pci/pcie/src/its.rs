// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GICv3 ITS interrupt wrappers for PCIe.
//!
//! The ITS routes MSIs using a 32-bit device ID. For PCIe, this is
//! `(segment << 16) | bdf`. The BDF is resolved by `MsiTarget` from
//! the port's bus range (for single-function devices) or passed
//! explicitly by multi-function devices and root ports.
//!
//! The wrappers here handle only the segment-to-ITS-devid mapping:
//! prepending the PCI segment number to the BDF. One instance per
//! root complex or switch connection.

use pal_event::Event;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;

/// A [`SignalMsi`] wrapper that prepends the PCI segment to the BDF,
/// producing the ITS device ID.
///
/// When `devid` is `Some(bdf)`, the ITS device ID is
/// `(segment << 16) | (bdf & 0xFFFF)`.
///
/// In the normal flow this wrapper is invoked via
/// [`MsiTarget::signal_msi`](pci_core::msi::MsiTarget::signal_msi) or
/// [`MsiTarget::signal_msi_with_rid`](pci_core::msi::MsiTarget::signal_msi_with_rid),
/// both of which always pass `Some(bdf)`, so the `None` arm is
/// unreachable. It is retained as a defensive guard for direct trait
/// callers that have no BDF to provide.
pub struct ItsSignalMsi {
    inner: Arc<dyn SignalMsi>,
    segment: u16,
}

impl ItsSignalMsi {
    /// Creates a new wrapper for the given segment.
    pub fn new(inner: Arc<dyn SignalMsi>, segment: u16) -> Self {
        Self { inner, segment }
    }
}

impl SignalMsi for ItsSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        let Some(bdf) = devid else {
            return;
        };
        let its_devid = (self.segment as u32) << 16 | (bdf & 0xFFFF);
        self.inner.signal_msi(Some(its_devid), address, data);
    }
}

/// An [`IrqFd`] wrapper that produces ITS-aware irqfd routes.
pub struct ItsIrqFd {
    inner: Arc<dyn IrqFd>,
    segment: u16,
}

impl ItsIrqFd {
    /// Creates a new wrapper for the given segment.
    pub fn new(inner: Arc<dyn IrqFd>, segment: u16) -> Self {
        Self { inner, segment }
    }
}

impl IrqFd for ItsIrqFd {
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
        let inner_route = self.inner.new_irqfd_route()?;
        Ok(Box::new(ItsIrqFdRoute {
            inner: inner_route,
            segment: self.segment,
        }))
    }
}

/// An [`IrqFdRoute`] wrapper that prepends the PCI segment to the BDF
/// on `enable`.
struct ItsIrqFdRoute {
    inner: Box<dyn IrqFdRoute>,
    segment: u16,
}

impl IrqFdRoute for ItsIrqFdRoute {
    fn event(&self) -> &Event {
        self.inner.event()
    }

    fn enable(&self, address: u64, data: u32, devid: Option<u32>) {
        let Some(bdf) = devid else {
            return;
        };
        let its_devid = (self.segment as u32) << 16 | (bdf & 0xFFFF);
        self.inner.enable(address, data, Some(its_devid));
    }

    fn disable(&self) {
        self.inner.disable();
    }
}
