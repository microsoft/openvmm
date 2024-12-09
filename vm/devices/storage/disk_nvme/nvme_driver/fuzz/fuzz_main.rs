// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_emulated_device;

use crate::fuzz_emulated_device::FuzzEmulatedDevice;

use arbitrary::{Arbitrary, Unstructured};
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::{NvmeController, NvmeControllerCaps};
use nvme_driver::{Namespace, NvmeDriver};
use nvme_spec::nvm::DsmRange;
use pal_async::{DefaultDriver, DefaultPool};
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use user_driver::emulated::{DeviceSharedMemory, EmulatedDevice};
use vmcore::vm_task::{SingleDriverBackend, VmTaskDriverSource};
use xtask_fuzz::fuzz_target;

const INPUT_LEN:usize=4196;

/// Writes the given arbitrary bytes to disk and reads arbitrary number of blocks from an arbitrary
/// address in the disk. The number of blocks being read can be larger than the provided memory.
///
/// TODO
fn do_fuzz(u: &mut Unstructured<'_>) {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // ---- SETUP ----
        let mut fuzzing_driver = FuzzNvmeDriver::new(driver).await;

        // ---- FUZZING ----
        while !u.is_empty() {
            let next_action = fuzzing_driver.get_arbitrary_action(u).unwrap();

            println!("{:x?}", next_action);

            fuzzing_driver.execute_action(next_action);
        }

        // ---- CLEANUP ----
        fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to call the do_fuzz function.
// TODO: Do I need to implement something with the corpus here? Seems like the corpus here would
// only indicate length of the input that is passed in which doesn't really make too much sense
fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(&mut Unstructured::new(input))
});


/// Struct that stores variables to fuzz the nvme driver
pub struct FuzzNvmeDriver{
    driver: Option<NvmeDriver<EmulatedDevice<NvmeController>>>,
    namespace: Namespace,  // TODO: This can be implemented as a queue to test 'create' for
    payload_mem: GuestMemory,
}

impl FuzzNvmeDriver {
    /// Setup a new fuzz driver that will
    pub async fn new(driver: DefaultDriver) -> Self {
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

        let device = FuzzEmulatedDevice::new(nvme, msi_set, mem, u);
        let nvme_driver = NvmeDriver::new(&driver_source, 64, device).await.unwrap();

        let namespace = nvme_driver.namespace(1).await.unwrap();

        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // Trasfer buffer
        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, INPUT_LEN as u64, false)
            .unwrap();


        Self {
            driver: Some(nvme_driver),
            namespace,
            payload_mem,
        }
    }

    /// Cleans up fuzzing infrastructure properly
    async fn shutdown(&self) {
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
    }

    /// Returns an arbitrary action to be taken. Along with arbitrary values
    pub fn get_arbitrary_action(&self, u: &mut Unstructured<'_>) -> arbitrary::Result<NvmeDriverAction>{
       let action: NvmeDriverAction = u.arbitrary()?; 
       Ok(action)
    }

    /// Executes an action
    pub async fn execute_action(&mut self, action: NvmeDriverAction) {
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
    }
}

impl Drop for FuzzNvmeDriver {
    // Takes ownership of the driver and gracefully shuts down upon drop
    fn drop(&mut self) {
        // TODO: Maybe call the shutdown() method during this phase as well
        self.driver.take().unwrap().shutdown();
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
