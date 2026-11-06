// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various NVMe behaviors.

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
use disk_backend::pr;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use scsi_buffers::RequestBuffers;
use std::collections::VecDeque;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

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

/// A disk whose first read blocks until the test releases it. Every read
/// reports on a channel that it has entered the disk; every I/O after the
/// first completes at once.
///
/// With reservations, a reservation report is gated the same way, and it
/// shares the one gate with reads.
#[derive(Inspect)]
pub struct GatedDisk {
    sectors: u64,
    reservations: bool,
    #[inspect(skip)]
    entered: mesh::Sender<()>,
    // Taken by the first read, so a later read does not wait on a spent gate.
    #[inspect(skip)]
    gate: Mutex<Option<mesh::OneshotReceiver<()>>>,
}

/// The test's handle on a [`GatedDisk`].
pub struct DiskGate {
    entered: mesh::Receiver<()>,
    release: Option<mesh::OneshotSender<()>>,
}

impl DiskGate {
    /// Waits until a read (or a reservation report) has entered the disk.
    /// Panics, naming the caller's line, if none does within
    /// [`TEST_WAIT_TIMEOUT`].
    #[track_caller]
    pub fn wait_entered(&mut self) -> impl Future<Output = ()> + '_ {
        let at = Location::caller();
        async move {
            match within_test_timeout(self.entered.next()).await {
                Some(Some(())) => {}
                Some(None) => {
                    panic!("disk dropped while waiting for a read to enter it (waited at {at})")
                }
                None => panic!(
                    "no read entered the gated disk within {TEST_WAIT_TIMEOUT:?} (waited at {at})"
                ),
            }
        }
    }

    /// Lets the held read (or report) complete.
    pub fn release(&mut self) {
        self.release.take().expect("released twice").send(());
    }

    /// Returns whether a read (or report) has entered the disk since the
    /// last check, without waiting. The caller must first let every woken
    /// task run, or the answer says nothing.
    pub fn try_entered(&mut self) -> bool {
        self.entered.try_recv().is_ok()
    }
}

/// Returns a [`GatedDisk`] of `sectors` 512-byte sectors, with persistent
/// reservations if `reservations`, wrapped as a [`Disk`], plus the test's
/// handle on its gate.
pub fn gated_disk(sectors: u64, reservations: bool) -> (Disk, DiskGate) {
    let (entered_send, entered_recv) = mesh::channel();
    let (release_send, release_recv) = mesh::oneshot();
    let disk = GatedDisk {
        sectors,
        reservations,
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

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.reservations
            .then_some(self as &dyn pr::PersistentReservation)
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> Result<(), DiskError> {
        self.pass_gate().await;
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

impl GatedDisk {
    /// Reports that an operation entered the disk, then waits for the gate
    /// if no earlier operation took it.
    async fn pass_gate(&self) {
        self.entered.send(());
        // Take the gate in its own statement so the lock guard is dropped
        // before the await.
        let gate = self.gate.lock().take();
        if let Some(gate) = gate {
            gate.await.expect("gate dropped");
        }
    }
}

#[async_trait::async_trait]
impl pr::PersistentReservation for GatedDisk {
    fn capabilities(&self) -> pr::ReservationCapabilities {
        pr::ReservationCapabilities {
            write_exclusive: true,
            exclusive_access: true,
            write_exclusive_registrants_only: true,
            exclusive_access_registrants_only: true,
            write_exclusive_all_registrants: true,
            exclusive_access_all_registrants: true,
            persist_through_power_loss: true,
        }
    }

    async fn report(&self) -> Result<pr::ReservationReport, DiskError> {
        self.pass_gate().await;
        Ok(pr::ReservationReport {
            generation: 0,
            reservation_type: None,
            // Set, so a Get Features of Reservation Persistence that got
            // this answer reads PTPL set in DW0, and one posted without it
            // does not.
            persist_through_power_loss: true,
            controllers: Vec::new(),
        })
    }

    async fn register(
        &self,
        _current_key: Option<u64>,
        _new_key: u64,
        _ptpl: Option<bool>,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    async fn reserve(
        &self,
        _key: u64,
        _reservation_type: pr::ReservationType,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    async fn release(
        &self,
        _key: u64,
        _reservation_type: pr::ReservationType,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    async fn clear(&self, _key: u64) -> Result<(), DiskError> {
        Ok(())
    }

    async fn preempt(
        &self,
        _current_key: u64,
        _preempt_key: u64,
        _reservation_type: pr::ReservationType,
        _abort: bool,
    ) -> Result<(), DiskError> {
        Ok(())
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
    /// Waits for the oldest unconsumed MSI with this data value, skipping
    /// any others. Panics, naming the skipped MSIs and the caller's line, if
    /// none arrives within [`TEST_WAIT_TIMEOUT`] or the tap closes.
    #[track_caller]
    pub fn next_with_data(&mut self, data: u32) -> impl Future<Output = ()> + '_ {
        let at = Location::caller();
        async move {
            let mut skipped = Vec::new();
            let found = within_test_timeout(async {
                while let Some((_, d)) = self.0.next().await {
                    if d == data {
                        return true;
                    }
                    skipped.push(d);
                }
                false
            })
            .await;
            match found {
                Some(true) => {}
                Some(false) => panic!(
                    "msi tap closed while waiting for MSI data {data:#x}, skipped {skipped:#x?} (waited at {at})"
                ),
                None => panic!(
                    "no MSI with data {data:#x} within {TEST_WAIT_TIMEOUT:?}, skipped {skipped:#x?} (waited at {at})"
                ),
            }
        }
    }

    /// Takes every MSI already in the tap, without waiting, and returns
    /// their data values, oldest first. The caller must first let every
    /// woken task run, or an empty answer says nothing.
    pub fn take_pending(&mut self) -> Vec<u32> {
        let mut taken = Vec::new();
        while let Ok((_, data)) = self.0.try_recv() {
            taken.push(data);
        }
        taken
    }
}

/// How long a test waits for an event the controller must produce before
/// failing. The defects these tests look for show up as an event that never
/// arrives, which an unbounded wait turns into a hang. The events come from
/// in-process emulation and arrive in milliseconds, so the bound is generous.
pub const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `fut` to completion, or returns `None` once [`TEST_WAIT_TIMEOUT`]
/// has passed.
pub async fn within_test_timeout<F: Future>(fut: F) -> Option<F::Output> {
    mesh::CancelContext::new()
        .with_timeout(TEST_WAIT_TIMEOUT)
        .until_cancelled(fut)
        .await
        .ok()
}

/// Runs `fut` to completion, panicking with `what` and the caller's line if
/// it takes longer than [`TEST_WAIT_TIMEOUT`].
///
/// Not an async fn, so that `#[track_caller]` sees the caller.
#[track_caller]
pub fn bounded<F: Future>(what: &'static str, fut: F) -> impl Future<Output = F::Output> {
    bounded_at(what, Location::caller(), fut)
}

/// [`bounded`] for a helper that waits on behalf of its own caller.
pub async fn bounded_at<F: Future>(
    what: &'static str,
    at: &'static Location<'static>,
    fut: F,
) -> F::Output {
    match within_test_timeout(fut).await {
        Some(out) => out,
        None => panic!("{what} within {TEST_WAIT_TIMEOUT:?} (waited at {at})"),
    }
}
