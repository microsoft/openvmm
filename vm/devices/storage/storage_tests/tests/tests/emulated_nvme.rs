// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared harness for tests that need a real `NvmeDisk`.
//!
//! Getting to an `NvmeDisk` means standing up a controller, an emulated PCI
//! device and a driver, none of which is interesting to the tests that need
//! one. This assembles that stack over a RAM-backed namespace.
//!
//! This lives alongside the tests rather than in the crate's library because it
//! pulls in the whole NVMe emulation stack. The library is deliberately kept to
//! three dependencies so that backend crates can call the conformance suites
//! from their own unit tests cheaply; putting this there would defeat that.

use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_backend::Disk;
use disk_nvme::NvmeDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::NamespaceHandle;
use nvme_driver::NvmeDriver;
use page_pool_alloc::PagePoolAllocator;
use pal_async::DefaultDriver;
use pci_core::bus_range::AssignedBusRange;
use pci_core::dma::DmaTarget;
use pci_core::msi::MsiConnection;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

const MSIX_COUNT: u16 = 2;
const IO_QUEUE_COUNT: u16 = 64;
const CPU_COUNT: u32 = 64;

/// An emulated NVMe controller with a single RAM-backed namespace.
///
/// The driver must outlive any disk built from its namespace, so callers must
/// keep this alive for as long as they use the disk.
pub struct EmulatedNvme {
    driver: NvmeDriver<EmulatedDevice<NvmeController, PagePoolAllocator>>,
    payload_mem: GuestMemory,
}

impl EmulatedNvme {
    /// Builds a controller whose namespace 1 is a RAM disk of
    /// `sector_size * sector_count` bytes.
    pub async fn new(
        driver: DefaultDriver,
        sector_size: u32,
        sector_count: u64,
        read_only: bool,
        mem_name: &str,
    ) -> Self {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        // First 4MB for the device and second 4MB for the payload.
        let pages = 1024;
        let mem = DeviceTestMemory::new(pages * 2, false, mem_name);
        let guest_mem = mem.guest_memory();
        let dma_client = mem.dma_client();
        let payload_mem = mem.payload_mem();

        let msi_conn = MsiConnection::new();
        let dma_target = DmaTarget::new(AssignedBusRange::new(), 0, guest_mem.clone(), &msi_conn);
        let nvme = NvmeController::new(
            &driver_source,
            &dma_target,
            &mut ExternallyManagedMmioIntercepts,
            NvmeControllerCaps {
                msix_count: MSIX_COUNT,
                max_io_queues: IO_QUEUE_COUNT,
                subsystem_id: Guid::new_random(),
            },
        );

        nvme.client()
            .add_namespace(
                1,
                disklayer_ram::ram_disk(sector_size as u64 * sector_count, read_only).unwrap(),
            )
            .await
            .unwrap();

        let device = EmulatedDevice::new(nvme, msi_conn, dma_client);
        let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false, false)
            .await
            .unwrap();

        Self {
            driver,
            payload_mem,
        }
    }

    /// Guest memory set aside for test payloads.
    pub fn payload_mem(&self) -> &GuestMemory {
        &self.payload_mem
    }

    /// Takes the handle to namespace 1.
    ///
    /// [`NamespaceHandle`] is deliberately not `Clone` — the driver relies on
    /// single ownership to know when a namespace is no longer in use — so this
    /// can only be used once.
    pub async fn namespace(&mut self) -> NamespaceHandle {
        self.driver.namespace(1).await.unwrap()
    }

    /// Takes namespace 1 as a [`Disk`]. See [`Self::namespace`] for why this
    /// can only be used once.
    pub async fn disk(&mut self) -> Disk {
        Disk::new(NvmeDisk::new(self.namespace().await)).unwrap()
    }
}
