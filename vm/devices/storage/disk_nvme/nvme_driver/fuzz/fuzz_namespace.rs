// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arbitrary::Arbitrary;
use guestmem::GuestMemory;
use nvme_driver::Namespace;
use nvme_spec::nvm::DsmRange;
use scsi_buffers::OwnedRequestBuffers;
use user_driver::emulated::DeviceSharedMemory;

// Number of random bytes to use when reading data
const INPUT_LEN:usize=4196;

pub struct FuzzNamespace {
    namespace: Namespace,
    payload_mem: GuestMemory,
}

impl FuzzNamespace {
    pub fn new(namespace: Namespace) -> Self {
        let base_len = 64 << 20;  // 64MB
        let payload_len = 1 << 20;  // 1MB
        let mem = DeviceSharedMemory::new(base_len, payload_len);

        // Trasfer buffer
        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, INPUT_LEN as u64, false)
            .unwrap();

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
    }

    /// Executes an action
    pub async fn execute_action(&self, action: NamespaceAction) {
        match action {
            NamespaceAction::Read { lba, block_count, target_cpu} => {
                self.read_arbitrary(lba, block_count, target_cpu).await
            }
            NamespaceAction::Write { lba, block_count, target_cpu } => {
                self.write_arbitrary(lba, block_count, target_cpu).await
            }
            NamespaceAction::Flush { target_cpu } => {
                self.flush_arbitrary(target_cpu).await
            }
        } 
    }
}

#[derive(Debug, Arbitrary)]
pub enum NamespaceAction {
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
