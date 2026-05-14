// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GICv3 ITS interrupt wrappers for PCIe devices.
//!
//! The ITS routes MSIs using a 32-bit device ID. For PCIe, this is `(segment <<
//! 16) | bdf`, where `bdf = (bus << 8) | (dev << 3) | fn`.
//!
//! [`ItsSignalMsi`] and [`ItsIrqFd`] wrap a partition's generic MSI and irqfd
//! implementations to inject the ITS device ID. The bus range comes from a
//! shared [`AssignedBusRange`] (updated by the PCIe port when the guest assigns
//! bus numbers); the segment is fixed at construction time.
//!
//! For single-function devices (`devid == None`), the wrapper defaults to
//! device 0, function 0 on the port's secondary bus. Multi-function devices
//! pass `Some(bdf)` where `bdf = (bus << 8) | (dev << 3) | fn`.

use crate::bus_range::AssignedBusRange;
use pal_event::Event;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;

/// A [`SignalMsi`] wrapper that composes the ITS device ID before
/// forwarding to the inner implementation.
pub struct ItsSignalMsi {
    inner: Arc<dyn SignalMsi>,
    bus_range: AssignedBusRange,
    segment: u16,
}

impl ItsSignalMsi {
    /// Creates a new wrapper.
    ///
    /// `segment` is the PCI segment number of the root complex that
    /// owns this device.
    pub fn new(inner: Arc<dyn SignalMsi>, bus_range: AssignedBusRange, segment: u16) -> Self {
        Self {
            inner,
            bus_range,
            segment,
        }
    }
}

impl SignalMsi for ItsSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        let Some(its_devid) = self.bus_range.compose_its_devid(self.segment, devid) else {
            return;
        };
        self.inner.signal_msi(Some(its_devid), address, data);
    }
}

/// An [`IrqFd`] wrapper that produces ITS irqfd routes, each
/// of which injects the ITS device ID into the `devid` parameter on
/// `enable`.
pub struct ItsIrqFd {
    inner: Arc<dyn IrqFd>,
    bus_range: AssignedBusRange,
    segment: u16,
}

impl ItsIrqFd {
    /// Creates a new wrapper.
    ///
    /// `segment` is the PCI segment number of the root complex that
    /// owns this device.
    pub fn new(inner: Arc<dyn IrqFd>, bus_range: AssignedBusRange, segment: u16) -> Self {
        Self {
            inner,
            bus_range,
            segment,
        }
    }
}

impl IrqFd for ItsIrqFd {
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
        let inner_route = self.inner.new_irqfd_route()?;
        Ok(Box::new(ItsIrqFdRoute {
            inner: inner_route,
            bus_range: self.bus_range.clone(),
            segment: self.segment,
        }))
    }
}

/// An [`IrqFdRoute`] wrapper that composes the ITS device ID on
/// `enable`.
struct ItsIrqFdRoute {
    inner: Box<dyn IrqFdRoute>,
    bus_range: AssignedBusRange,
    segment: u16,
}

impl IrqFdRoute for ItsIrqFdRoute {
    fn event(&self) -> &Event {
        self.inner.event()
    }

    fn enable(&self, address: u64, data: u32, devid: Option<u32>) {
        let Some(its_devid) = self.bus_range.compose_its_devid(self.segment, devid) else {
            return;
        };
        self.inner.enable(address, data, Some(its_devid));
    }

    fn disable(&self) {
        self.inner.disable();
    }
}
