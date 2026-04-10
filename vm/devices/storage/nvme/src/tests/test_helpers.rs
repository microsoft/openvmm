// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various NVMe behaviors.

use crate::PAGE_SIZE;
use crate::PAGE_SIZE64;
use crate::prp::PrpRange;
use crate::spec;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use guestmem::GuestMemory;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
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
    msi_requests: Mutex<VecDeque<(u64, u32)>>, // (addr, data)
}

impl TestPciInterruptController {
    /// Return a new test PCI interrupt controller
    #[expect(dead_code)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TestPciInterruptControllerInner {
                msi_requests: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Fetch the first (addr, data) MSI-X interrupt in the FIFO interrupt queue
    #[expect(dead_code)]
    pub fn get_next_interrupt(&self) -> Option<(u64, u32)> {
        self.inner.msi_requests.lock().pop_front()
    }
}

impl SignalMsi for TestPciInterruptController {
    fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        assert_eq!(rid, 0);
        self.inner.msi_requests.lock().push_back((address, data));
    }
}

pub fn test_memory() -> GuestMemory {
    GuestMemory::allocate(PAGE_SIZE * 64)
}

pub struct TestNvmeMmioRegistration {}

/// A trait to register device-specific IO intercept regions.
impl RegisterMmioIntercept for TestNvmeMmioRegistration {
    /// Registers a new IO region of the given length.
    fn new_io_region(&mut self, _debug_name: &str, _len: u64) -> Box<dyn ControlMmioIntercept> {
        Box::new(TestNvmeControlMmioIntercept::new())
    }
}

pub struct TestNvmeControlMmioIntercept {}

impl TestNvmeControlMmioIntercept {
    pub fn new() -> TestNvmeControlMmioIntercept {
        TestNvmeControlMmioIntercept {}
    }
}

/// A trait to map/unmap a device-specific IO memory region.
impl ControlMmioIntercept for TestNvmeControlMmioIntercept {
    /// Enables the IO region at the given address.
    ///
    /// This method will never fail, as devices are not expected to gracefully
    /// handle the case where an IO region overlaps with an existing region.
    fn map(&mut self, _addr: u64) {}

    /// Disables the IO region.
    fn unmap(&mut self) {}

    /// Return the currently mapped address.
    ///
    /// Returns `None` if the region is currently unmapped.
    fn addr(&self) -> Option<u64> {
        None
    }

    fn len(&self) -> u64 {
        8096
    }

    /// Return the offset of `addr` from the region's base address.
    ///
    /// Returns `None` if the provided `addr` is outside of the memory
    /// region, or the region is currently unmapped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let foo_region = register.new_io_region("foo", 0x10);
    /// foo_region.map(0x1000);
    /// assert_eq!(foo_region.offset_of(0x1003), Some(3));
    /// assert_eq!(foo_region.offset_of(0x900), None);
    /// assert_eq!(foo_region.offset_of(0x1020), None);
    /// foo_region.unmap();
    /// assert_eq!(foo_region.offset_of(0x1003), None);
    /// ```
    fn offset_of(&self, _addr: u64) -> Option<u64> {
        None
    }

    fn region_name(&self) -> &str {
        "???"
    }
}

pub fn write_command_to_queue(
    gm: &GuestMemory,
    dm: &PrpRange,
    slot: usize,
    command: &spec::Command,
) {
    let offset_in_queue = slot * size_of::<spec::Command>();
    let page_in_queue = offset_in_queue / PAGE_SIZE64 as usize;
    let offset_in_page = offset_in_queue % PAGE_SIZE64 as usize;
    let gpa = (dm.range().gpns()[page_in_queue] * PAGE_SIZE64) + offset_in_page as u64;

    gm.write_plain::<spec::Command>(gpa, command).unwrap();
}

pub fn read_completion_from_queue(
    gm: &GuestMemory,
    dm: &PrpRange,
    slot: usize,
) -> spec::Completion {
    let offset_in_queue = slot * size_of::<spec::Completion>();
    let page_in_queue = offset_in_queue / PAGE_SIZE64 as usize;
    let offset_in_page = offset_in_queue % PAGE_SIZE64 as usize;
    let gpa = (dm.range().gpns()[page_in_queue] * PAGE_SIZE64) + offset_in_page as u64;

    gm.read_plain::<spec::Completion>(gpa).unwrap()
}
