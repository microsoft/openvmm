// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::{Arbitrary, Unstructured};
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guid::Guid;
use nvme::NvmeControllerCaps;
use nvme_driver::NvmeDriver;
use nvme_spec::nvm::DsmRange;
use pal_async::DefaultPool;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use xtask_fuzz::fuzz_target;

// Number of random bytes to use when reading data
const INPUT_LEN:usize=4196;

/// Struct for input from the fuzzer
/// * `data` - Array of arbitrary bytes
/// * `lba` - Logical Block Address to read from
/// * `block_count` - Number of block to read in
#[derive(Debug)]
struct FuzzInput {
    data: [u8 ; INPUT_LEN],
    lba: u64,
    block_count: u32
}

/// Implements the Arbitrary trait for the FuzzInput struct
impl<'a> Arbitrary<'a> for FuzzInput {
    fn arbitrary(unstructured: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
       // Input array of size INPUT_LEN where all elements are 0
       let mut input = [0u8 ; INPUT_LEN];

       // Fill the FuzzerInput struct with arbitrary data and return
       unstructured.fill_buffer(&mut input).unwrap_or_default();
       Ok(FuzzInput {
           data: input,
           lba: u64::arbitrary(unstructured).unwrap(),
           block_count: u32::arbitrary(unstructured).unwrap()
       })

    }
}

/// Uses the input data from the FuzzInput and uses the nvmedriver to write the data bytes to disk
/// and then read the written bytes back in.
///
/// # Arguments
/// * `input` - An arbitrary(see function above) FuzzInput struct
fn do_fuzz(input: FuzzInput) {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        
        // ----- MEMORY SETUP ----:
        // Creates required memory areas for shared memory and RamDisk for the NVME Namespace
        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // Trasfer buffer
        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, INPUT_LEN as u64, false)
            .unwrap();

        // Request buffer defiition, the actual buffer will be created later.
        let buf_range = OwnedRequestBuffers::linear(0, 16384, true);

        // Back the NVME Driver
        let ram_disk = RamDisk::new(1 << 20, false).unwrap();

        // Write the input bytes to the payload memory 
        payload_mem.write_at(0, &input.data).unwrap();

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
        
        // Create our namespace
        nvme.client()
            .add_namespace(1, Arc::new(ram_disk))
            .await
            .unwrap();

        let device = EmulatedDevice::new(nvme, msi_set, mem);
        let nvme_driver = NvmeDriver::new(&driver_source, 64, device).await.unwrap();

        // Use this to read and write to the namespace
        let namespace = nvme_driver.namespace(1).await.unwrap();

        // ---- FUZZING ----
        // Write to the first block of the namespace
        namespace
            .write(
                0,
                0,
                2,
                false,
                &payload_mem,
                buf_range.buffer(&payload_mem).range(),
            )
            .await
            .unwrap();
        
        // Read from then namespace from arbitrary address and arbitrary amount of data
        namespace
            .read(
                1,
                input.lba,
                input.block_count,
                &payload_mem,
                buf_range.buffer(&payload_mem).range(),
            )
            .await
            .unwrap();

        // ---- CLEANUP ----
        // Deallocate the namespace and shut down the driver
        namespace
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

        nvme_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to call the do_fuzz function.
fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
