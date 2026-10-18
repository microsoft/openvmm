// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various NVMe behaviors.

use crate::NvmeController;
use crate::PAGE_SIZE;
use crate::PAGE_SIZE64;
use crate::prp::PrpRange;
use crate::spec;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use scsi_buffers::RequestBuffers;
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
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
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

const GATED_SECTOR_SIZE: u32 = 512;

/// A disk whose first read blocks until the test releases it.
#[derive(Inspect)]
pub struct GatedDisk {
    sectors: u64,
    #[inspect(skip)]
    entered: mesh::Sender<()>,
    #[inspect(skip)]
    gate: Mutex<Option<mesh::OneshotReceiver<()>>>,
}

/// The test's handle on a [`GatedDisk`].
pub struct DiskGate {
    entered: mesh::Receiver<()>,
    release: Option<mesh::OneshotSender<()>>,
}

impl DiskGate {
    /// Blocks until a read has entered the disk.
    pub async fn wait_entered(&mut self) {
        self.entered.next().await.expect("disk dropped");
    }

    /// Lets the held read complete.
    pub fn release(&mut self) {
        self.release.take().expect("released twice").send(());
    }

    /// Returns whether a read has entered the disk since the last check,
    /// without waiting.
    pub fn try_entered(&mut self) -> bool {
        self.entered.try_recv().is_ok()
    }
}

/// Returns a [`GatedDisk`] of `sectors` 512-byte sectors, wrapped as a
/// [`Disk`], plus the test's handle on its gate.
pub fn gated_disk(sectors: u64) -> (Disk, DiskGate) {
    let (entered_send, entered_recv) = mesh::channel();
    let (release_send, release_recv) = mesh::oneshot();
    let disk = GatedDisk {
        sectors,
        entered: entered_send,
        gate: Mutex::new(Some(release_recv)),
    };
    (
        Disk::new(disk).unwrap(),
        DiskGate {
            entered: entered_recv,
            release: Some(release_send),
        },
    )
}

impl DiskIo for GatedDisk {
    fn disk_type(&self) -> &str {
        "gated"
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn sector_size(&self) -> u32 {
        GATED_SECTOR_SIZE
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        GATED_SECTOR_SIZE
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> Result<(), DiskError> {
        self.entered.send(());
        let gate = self.gate.lock().take();
        if let Some(gate) = gate {
            gate.await.expect("gate dropped");
        }
        buffers.writer().zero(buffers.len())?;
        Ok(())
    }

    async fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }
}

/// Forwards every MSI to an inner target and to a channel.
pub struct TeeMsi {
    pub inner: Arc<dyn SignalMsi>,
    pub tap: mesh::Sender<(u64, u32)>,
}

impl SignalMsi for TeeMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        self.inner.signal_msi(devid, address, data);
        self.tap.send((address, data));
    }
}

/// The receiving end of a [`TeeMsi`], as `(address, data)` pairs.
pub struct MsiTap(pub mesh::Receiver<(u64, u32)>);

impl MsiTap {
    /// Waits for the next MSI with this data value, skipping any others.
    pub async fn next_with_data(&mut self, data: u32) {
        while let Some((_, d)) = self.0.next().await {
            if d == data {
                return;
            }
        }
        panic!("msi tap closed");
    }
}

/// Returns once every task the controller has already woken has run.
///
/// This is an inspect round trip through the coordinator, which the executor
/// runs only after the tasks already queued ahead of it.
pub async fn settle(nvmec: &mut NvmeController) {
    inspect::inspect("", nvmec).resolve().await;
}
