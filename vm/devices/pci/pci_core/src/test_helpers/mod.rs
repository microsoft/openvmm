// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various PCI behaviors.

use crate::msi::SignalMsi;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// A test-only interrupt controller that simply stashes incoming interrupt
/// requests in a FIFO queue. Implements [`SignalMsi`].
#[derive(Debug, Clone)]
pub struct TestPciInterruptController {
    inner: Arc<TestPciInterruptControllerInner>,
}

#[derive(Debug)]
struct TestPciInterruptControllerInner {
    // TODO: also support INTx interrupts
    msi_requests: Mutex<VecDeque<(u64, u32)>>, // (addr, data)
}

impl TestPciInterruptController {
    /// Return a new test PCI interrupt controller
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TestPciInterruptControllerInner {
                msi_requests: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Fetch the first (addr, data) MSI-X interrupt in the FIFO interrupt queue
    pub fn get_next_interrupt(&self) -> Option<(u64, u32)> {
        self.inner.msi_requests.lock().pop_front()
    }

    /// Returns an `Arc<dyn SignalMsi>` to this controller.
    pub fn signal_msi(&self) -> Arc<dyn SignalMsi> {
        self.inner.clone()
    }
}

impl SignalMsi for TestPciInterruptControllerInner {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        self.msi_requests.lock().push_back((address, data));
    }
}

/// Read a u32 from a `PciCapability`.
pub fn read_cap_u32(cap: &impl crate::capabilities::PciCapability, offset: u16) -> u32 {
    let mut value = 0;
    cap.read(
        offset,
        chipset_device::pci::ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
    );
    value
}

/// Write a u32 to a `PciCapability`.
pub fn write_cap_u32(cap: &mut impl crate::capabilities::PciCapability, offset: u16, val: u32) {
    cap.write(
        offset,
        chipset_device::pci::ByteEnabledDwordWrite::with_all_bytes_enabled(val),
    )
}

/// Read a u32 from a `PciExtendedCapability`.
pub fn read_extended_cap_u32(
    cap: &impl crate::capabilities::extended::PciExtendedCapability,
    offset: u16,
) -> u32 {
    let mut value = 0;
    cap.read(
        offset,
        chipset_device::pci::ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
    );
    value
}

/// Write a u32 to a `PciExtendedCapability`.
pub fn write_extended_cap_u32(
    cap: &mut impl crate::capabilities::extended::PciExtendedCapability,
    offset: u16,
    val: u32,
) {
    cap.write(
        offset,
        chipset_device::pci::ByteEnabledDwordWrite::with_all_bytes_enabled(val),
    )
}
