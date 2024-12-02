// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arbitrary::Arbitrary;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::Namespace;
use nvme_driver::NvmeDriver;
use pal_async::DefaultDriver;
use pci_core::msi::MsiInterruptSet;
use std::sync::Arc;
use user_driver::emulated::DeviceSharedMemory;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use crate::fuzz_emulated_device::FuzzEmulatedDevice;

pub struct FuzzDriver {
    driver: Option<NvmeDriver<FuzzEmulatedDevice<NvmeController>>>,
}

impl FuzzDriver {
    pub async fn new(driver: DefaultDriver) -> (Namespace, FuzzEmulatedDevice<NvmeController>, Self) {
        // Physical storage to back the disk
        let ram_disk = RamDisk::new(1 << 20, false).unwrap();

        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // ---- NVME DEVICE & DRIVER SETUP ----
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
        
        // Create nvme namespace
        nvme.client()
            .add_namespace(1, Arc::new(ram_disk))
            .await
            .unwrap();

        let device = FuzzEmulatedDevice::new(nvme, msi_set, mem);
        let nvme_driver = NvmeDriver::new(&driver_source, 64, device).await.unwrap();

        let namespace = nvme_driver.namespace(1).await.unwrap();

        (namespace,
         device,
         Self {
            driver: Some(nvme_driver),
         })
    }

    /// Executes an action
    pub async fn execute_action(&mut self, action: DriverAction) {
        match action {
            DriverAction::UpdateServicingFlags { nvme_keepalive } => {
                self.driver.as_mut().unwrap().update_servicing_flags(nvme_keepalive)
            }
        } 
    }
}

impl Drop for FuzzDriver {
    // Takes ownership of the driver and gracefully shuts down upon drop
    fn drop(&mut self) {
        self.driver.take().unwrap().shutdown();
    }

}

#[derive(Debug, Arbitrary)]
pub enum DriverAction {
    UpdateServicingFlags {
        nvme_keepalive: bool,
    }
}
