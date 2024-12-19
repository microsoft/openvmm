// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An interface to fuzz the nvme driver with arbitrary actions
use crate::fuzz_emulated_device::FuzzEmulatedDevice;
use crate::arbitrary_data;

use arbitrary::Arbitrary;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::Namespace;
use nvme_driver::NvmeDriver;
use nvme_spec::nvm::DsmRange;
use pal_async::DefaultDriver;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use user_driver::emulated::DeviceSharedMemory;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// Nvme driver fuzzer
pub struct FuzzNvmeDriver {
    driver: Option<NvmeDriver<FuzzEmulatedDevice<NvmeController>>>,
    namespace: Namespace,
    payload_mem: GuestMemory,
}

impl FuzzNvmeDriver {
    /// Setup a new nvme driver with a fuzz-enabled backend device.
    pub async fn new(driver: DefaultDriver) -> Self {
        // Physical storage to back the disk
        let ram_disk = RamDisk::new(1 << 20, false).unwrap();

        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // Trasfer buffer
        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, payload_len  as u64, false)
            .unwrap();

        // Nvme device and driver setup
        let driver_source =
            VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut msi_set = MsiInterruptSet::new();
        let nvme = NvmeController::new(
            &driver_source,
            mem.guest_memory().clone(),
            &mut msi_set,
            &mut ExternallyManagedMmioIntercepts,
            NvmeControllerCaps {
                msix_count: 2,
                max_io_queues: 64,
                subsystem_id: Guid::new_random(),
            },
        );

        nvme.client()
            .add_namespace(1, Arc::new(ram_disk))
            .await
            .unwrap();

        let device = FuzzEmulatedDevice::new(nvme, msi_set, mem);
        let nvme_driver = NvmeDriver::new(&driver_source, 64, device).await.unwrap();
        let namespace = nvme_driver.namespace(1).await.unwrap();

        Self {
            driver: Some(nvme_driver),
            namespace,
            payload_mem,
        }
    }

    /// Clean up fuzzing infrastructure.
    pub async fn shutdown(&mut self) {
        self.namespace
            .deallocate(
                0,
                &[
                    DsmRange {
                        context_attributes: 0,
                        starting_lba: 1000,
                        lba_count: 2000,
                    },
                    DsmRange {
                        context_attributes: 0,
                        starting_lba: 2,
                        lba_count: 2,
                    },
                ],
            )
            .await
            .unwrap();

        self.driver.take().unwrap().shutdown().await;
    }

    /// Generates and executes an arbitrary NvmeDriverAction. Returns either an arbitrary error or the executed action.
    pub async fn execute_arbitrary_action(&mut self) -> Result<(), arbitrary::Error> {
        let action = arbitrary_data::<NvmeDriverAction>()?;

        match action {
            NvmeDriverAction::Read { lba, block_count, target_cpu} => {
                let buf_range = OwnedRequestBuffers::linear(0, 16384, true);
                self.namespace
                    .read(
                        target_cpu,
                        lba,
                        block_count,
                        &self.payload_mem,
                        buf_range.buffer(&self.payload_mem).range(),
                    ).await.unwrap();
            }

            NvmeDriverAction::Write { lba, block_count, target_cpu } => {
                let buf_range = OwnedRequestBuffers::linear(0, 16384, true);
                self.namespace
                    .write(
                        target_cpu,
                        lba,
                        block_count,
                        false,
                        &self.payload_mem,
                        buf_range.buffer(&self.payload_mem).range(),
                    ).await.unwrap();
            }

            NvmeDriverAction::Flush { target_cpu } => {
                self.namespace
                    .flush(
                        target_cpu
                    ).await.unwrap();        
            }

            NvmeDriverAction::UpdateServicingFlags { nvme_keepalive } => {
                self.driver.as_mut().unwrap().update_servicing_flags(nvme_keepalive)
            }
        } 

        Ok(())
    }
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
