// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An interface to fuzz the nvme driver with arbitrary actions
use crate::arbitrary_data;
use crate::fuzz_emulated_device::FuzzEmulatedDevice;

use arbitrary::Arbitrary;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::Namespace;
use nvme_driver::NvmeDriver;
use nvme_spec::nvm::DsmRange;
use page_pool_alloc::PagePoolAllocator;
use pal_async::DefaultDriver;
use pci_core::msi::MsiConnection;
use scsi_buffers::OwnedRequestBuffers;
use std::convert::TryFrom;
use std::sync::Arc;
use user_driver_emulated_mock::DeviceTestMemory;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// Nvme driver fuzzer
pub struct FuzzNvmeDriver {
    driver: Option<NvmeDriver<FuzzEmulatedDevice<NvmeController, PagePoolAllocator>>>,
    namespace: Arc<Namespace>,
    payload_mem: GuestMemory,
    cpu_count: u32,
}

impl FuzzNvmeDriver {
    /// Setup a new nvme driver with a fuzz-enabled backend device.
    pub async fn new(driver: DefaultDriver) -> Result<Self, anyhow::Error> {
        let cpu_count = 64; // TODO: [use-arbitrary-input]
        let pages = 512; // 2MB TODO: [use-arbitrary-input]
        let mem = DeviceTestMemory::new(pages, false, "fuzz_nvme_driver");

        // Transfer buffer
        let payload_mem = mem.payload_mem();

        // Nvme device and driver setup
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let msi_conn = MsiConnection::new();

        let guid = arbitrary_guid()?;
        let nvme = NvmeController::new(
            &driver_source,
            mem.guest_memory().clone(),
            msi_conn.target(),
            &mut ExternallyManagedMmioIntercepts,
            NvmeControllerCaps {
                msix_count: 2,     // TODO: [use-arbitrary-input]
                max_io_queues: 64, // TODO: [use-arbitrary-input]
                subsystem_id: guid,
            },
        );

        nvme.client()
            .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap()) // TODO: [use-arbitrary-input]
            .await
            .unwrap();

        let device = FuzzEmulatedDevice::new(nvme, msi_conn, mem.dma_client());
        let mut nvme_driver = NvmeDriver::new(&driver_source, cpu_count, device, false).await?; // TODO: [use-arbitrary-input]
        let namespace = nvme_driver.namespace(1).await?; // TODO: [use-arbitrary-input]

        Ok(Self {
            driver: Some(nvme_driver),
            namespace,
            payload_mem,
            cpu_count,
        })
    }

    /// Clean up fuzzing infrastructure.
    pub async fn shutdown(&mut self) {
        self.namespace
            .deallocate(
                0, // TODO: [use-arbitrary-input]
                &[
                    DsmRange {
                        context_attributes: 0, // TODO: [use-arbitrary-input]
                        starting_lba: 1000,    // TODO: [use-arbitrary-input]
                        lba_count: 2000,       // TODO: [use-arbitrary-input]
                    },
                    DsmRange {
                        context_attributes: 0, // TODO: [use-arbitrary-input]
                        starting_lba: 2,       // TODO: [use-arbitrary-input]
                        lba_count: 2,          // TODO: [use-arbitrary-input]
                    },
                ],
            )
            .await
            .unwrap();

        self.driver.take().unwrap().shutdown().await;
    }

    /// Generates and executes an arbitrary NvmeDriverAction. `NvmeDriver` is not intended to be an interface
    /// consumed by anything ther than `NvmeDisk`, yet the fuzzer targets the driver directly.
    /// In addition, the `NvmeDriver` is not at any trust boundary. This is done to allow
    /// for providing arbitrary data in more places. However, you'll notice that many
    /// of these actions do sanitize the arbitrary data to some degree, to get past
    /// contract-checking `assert!`s in the driver. This does not break the goal of fuzzing,
    /// since these contract checks imply programmer error and the primary goal of this is
    /// to drive actions to get invalid data back from the underlying PCI & NVMe emulators.
    ///
    /// All that being said, be careful when deciding to sanitize inputs here: consider
    /// and explicitly rule out adding graceful error handling in the `NvmeDriver` itself.
    pub async fn execute_arbitrary_action(&mut self) -> Result<(), anyhow::Error> {
        let action = arbitrary_data::<NvmeDriverAction>()?;

        match action {
            NvmeDriverAction::Read {
                lba,
                block_count,
                target_cpu,
            } => {
                let buf_range = OwnedRequestBuffers::linear(0, 16384, true); // TODO: [use-arbitrary-input]
                self.namespace
                    .read(
                        // TODO: [panic-or-bail-on-fuzz]
                        target_cpu % self.cpu_count,
                        lba,
                        // TODO: [panic-or-bail-on-fuzz]
                        block_count
                            % (u32::try_from(buf_range.len()).unwrap()
                                >> self.namespace.block_size().trailing_zeros())
                            % self.namespace.max_transfer_block_count(),
                        &self.payload_mem,
                        buf_range.buffer(&self.payload_mem).range(),
                    )
                    .await?;
            }

            NvmeDriverAction::Write {
                lba,
                block_count,
                target_cpu,
            } => {
                let buf_range = OwnedRequestBuffers::linear(0, 16384, true); // TODO: [use-arbitrary-input]
                self.namespace
                    .write(
                        // TODO: [panic-or-bail-on-fuzz]
                        target_cpu % self.cpu_count,
                        lba,
                        // TODO: [panic-or-bail-on-fuzz]
                        block_count
                            % (u32::try_from(buf_range.len()).unwrap()
                                >> self.namespace.block_size().trailing_zeros())
                            % self.namespace.max_transfer_block_count(),
                        false,
                        &self.payload_mem,
                        buf_range.buffer(&self.payload_mem).range(),
                    )
                    .await?;
            }

            NvmeDriverAction::Flush { target_cpu } => {
                self.namespace.flush(target_cpu % self.cpu_count).await?; // TODO: [panic-or-bail-on-fuzz]
            }

            NvmeDriverAction::UpdateServicingFlags { nvme_keepalive } => {
                self.driver
                    .as_mut()
                    .unwrap()
                    .update_servicing_flags(nvme_keepalive);
            }
        }

        Ok(())
    }
}

/// Returns a Guid with arbitrary bytes or an error if there isn't enought arbitrary data left
fn arbitrary_guid() -> Result<Guid, arbitrary::Error> {
    let mut guid: Guid = Guid::new_random();

    guid.data1 = arbitrary_data::<u32>()?;
    guid.data2 = arbitrary_data::<u16>()?;
    guid.data3 = arbitrary_data::<u16>()?;

    for byte in &mut guid.data4 {
        *byte = arbitrary_data::<u8>()?;
    }

    Ok(guid)
}

#[derive(Debug, Arbitrary)]
pub enum NvmeDriverAction {
    Read {
        lba: u64,
        block_count: u32,
        target_cpu: u32,
    },
    Write {
        lba: u64,
        block_count: u32,
        target_cpu: u32,
    },
    Flush {
        target_cpu: u32,
    },
    UpdateServicingFlags {
        nvme_keepalive: bool,
    },
}
