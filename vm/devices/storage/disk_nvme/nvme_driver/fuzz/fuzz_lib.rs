// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arbitrary::{Arbitrary, Unstructured};
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeControllerCaps;
use nvme_driver::Namespace;
use nvme_driver::NvmeDriver;
use nvme_spec::nvm::DsmRange;
use pal_async::DefaultDriver;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

// Number of random bytes to use when reading data
const INPUT_LEN:usize=4196;


/// Struct that stores variables to fuzz the nvme driver
pub struct FuzzNvmeDriver {
    namespace: Namespace,
    payload_mem: GuestMemory,
}

impl FuzzNvmeDriver {
    /// Setup a new fuzz driver that will
    pub async fn new(driver: DefaultDriver) -> Self {
        // Creates required memory areas for shared memory and RamDisk for the NVME Namespace
        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // Trasfer buffer
        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, INPUT_LEN as u64, false)
            .unwrap();

        // Back the NVME Driver
        let ram_disk = RamDisk::new(1 << 20, false).unwrap();

        // ---- NVME DEVICE & DRIVER SETUP ----
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut msi_set = MsiInterruptSet::new();
        let nvme = nvme::NvmeController::new(
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

        let device = EmulatedDevice::new(nvme, msi_set, mem);
        let nvme_driver = NvmeDriver::new(&driver_source, 64, device).await.unwrap();

        let namespace = nvme_driver.namespace(1).await.unwrap();

        Self {
            namespace,
            payload_mem,
        }
    }

    /** Runs the read function using the given namespace
     *
     * # Arguments
     * * `lba` - The logical block address where to read from
     * * `block_count` - Number of blocks to read
     */
    pub async fn read_arbitrary(&self, lba: u64, block_count: u32, target_cpu: u32) {
        // TODO: What if the size of this buffer needs to be moved around? What then? Maybe look in
        // to the payload_mem and see what is going on.
        // Request buffer defiition, the actual buffer will be created later.
        let buf_range = OwnedRequestBuffers::linear(0, 16384, true);

        // Read from then namespace from arbitrary address and arbitrary amount of data
        self.namespace
            .read(
                target_cpu,
                lba,
                block_count,
                &self.payload_mem,
                buf_range.buffer(&self.payload_mem).range(),
            )
            .await
            .unwrap();
    }

    /** Runs the write function using the given namespace
     *
     * # Arguments
     * * `lba` - The logical block address where to read from
     * * `block_count` - Number of blocks to read
     */
    pub async fn write_arbitrary(&self, lba: u64, block_count: u32, target_cpu: u32) {
        // Request buffer defiition, the actual buffer will be created later.
        let buf_range = OwnedRequestBuffers::linear(0, 16384, true);

        // Write to the namespace from arbitrary passed in address and arbitrary amount of data.
        self.namespace
            .write(
                target_cpu,
                lba,
                block_count,
                false,
                &self.payload_mem,
                buf_range.buffer(&self.payload_mem).range(),
            )
            .await
            .unwrap();        
    }

    /** Flushes the provided_target CPU
     *
     * # Arguments
     * * `target_cpu` - The CPU to flush
     */
    pub async fn flush_arbitrary(&self, target_cpu: u32) {
        // Flush CPU
        self.namespace
            .flush(
                target_cpu
            )
            .await
            .unwrap();        
    }

    /// Cleans up the driver.
    pub async fn shutdown(&self) {
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

        // Put this in later
        // TODO: nvme_driver.shutdown().await;
    }

    /// Returns an arbitrary action to be taken. Along with arbitrary values
    pub fn get_arbitrary_action(&self, u: &mut Unstructured<'_>) -> arbitrary::Result<NvmeDriverAction>{
       let action: NvmeDriverAction = u.arbitrary()?; 
       Ok(action)
    }

    /// Executes an action
    pub async fn execute_action(&self, action: NvmeDriverAction) {
        match action {
            NvmeDriverAction::Read { lba, block_count, target_cpu} => {
                self.read_arbitrary(lba, block_count, target_cpu).await
            }
            NvmeDriverAction::Write { lba, block_count, target_cpu } => {
                self.write_arbitrary(lba, block_count, target_cpu).await
            }
            NvmeDriverAction::Flush { target_cpu } => {
                self.flush_arbitrary(target_cpu).await
            }
        } 
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
    }
}
