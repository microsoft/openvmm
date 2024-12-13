// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_emulated_device;

use crate::fuzz_emulated_device::FuzzEmulatedDevice;

use std::mem;
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
use std::sync::{Mutex, MutexGuard};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref VEC_FRONTEND: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    pub static ref VEC_BACKEND: Mutex<Vec<u8>> = Mutex::new(Vec::new());
}

const INPUT_LEN:usize=4196;

#[derive(Debug)]
pub struct LargeVec {
    pub vec: Vec<u8>
}

impl<'a> Arbitrary<'a> for LargeVec {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut vec = Vec::new();
        while vec.len() < 4000 {
            vec.push(u.arbitrary()?);
        }
        Ok(LargeVec {
            vec,
        })
    }   
}

/// Writes the given arbitrary bytes to disk and reads arbitrary number of blocks from an arbitrary
/// address in the disk. The number of blocks being read can be larger than the provided memory.
///
/// TODO
fn do_fuzz() {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // ---- SETUP ----
        let mut fuzzing_driver = FuzzNvmeDriver::new(driver).await;

        {
            println!("Do Fuzz Called with {} bytes", VEC_FRONTEND.lock().unwrap().len());
        }

        // ---- FUZZING ----
        loop {
            {
                if VEC_FRONTEND.lock().unwrap().is_empty() {
                    break;
                }
            }

            let next_action = fuzzing_driver.get_arbitrary_action();

            match next_action {
                Ok(action) => {
                    // println!("{:x?}", action);
                    // fuzzing_driver.execute_action(action).await;
                },
                Err(_e) => {
                    break;
                }
            }
        }

        // ---- CLEANUP ----
        // fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to call the do_fuzz function.
// TODO: Do I need to implement something with the corpus here? Seems like the corpus here would
// only indicate length of the input that is passed in which doesn't really make too much sense
fuzz_target!(|input: LargeVec| {
    xtask_fuzz::init_tracing_if_repro();
    let (input_frontend, input_backend) = input.vec.split_at(input.vec.len() / 2);

    {
    let mut vec_frontend = VEC_FRONTEND.lock().unwrap();
    *vec_frontend = input_frontend.to_vec();
    }

    {
    let mut vec_backend = VEC_BACKEND.lock().unwrap();
    *vec_backend = input_backend.to_vec();
    }

    do_fuzz();
});


/// Struct that stores variables to fuzz the nvme driver
pub struct FuzzNvmeDriver {
    driver: Option<NvmeDriver<FuzzEmulatedDevice<NvmeController>>>,
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

        let device = FuzzEmulatedDevice::new(nvme, msi_set, mem);

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
    pub fn get_arbitrary_action(&self) -> arbitrary::Result<NvmeDriverAction>{
        // Number of bytes we need to remove from the vector:
        let num_bytes = size_of::<NvmeDriverAction>();
        let action;

        let mut vec_frontend = VEC_FRONTEND.lock().unwrap();

        if vec_frontend.len() < num_bytes {
            println!("Not enough data");
            return Err(arbitrary::Error::NotEnoughData);
        } else {
            println!("Consuming {} bytes", num_bytes);
        }

        let drained: Vec<u8> = vec_frontend.drain(0..num_bytes).collect();
        let mut u = Unstructured::new(&drained);

        action = u.arbitrary()?;
        return Ok(action);
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

// impl Drop for FuzzNvmeDriver {
//     // Takes ownership of the driver and gracefully shuts down upon drop
//     fn drop(&mut self) {
//         // TODO: Maybe call the shutdown() method during this phase as well
//         self.driver.take().unwrap().shutdown();
//     }
// 
// }

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
