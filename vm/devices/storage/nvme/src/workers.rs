// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe admin and IO queue workers.

mod admin;
mod coordinator;
mod io;

pub use admin::NsidConflict;
pub use coordinator::NvmeControllerClient;
pub use coordinator::NvmeWorkers;

use crate::PAGE_SIZE;
use inspect::Inspect;

#[derive(Debug, Copy, Clone, Inspect, Default)]
pub struct IoQueueEntrySizes {
    pub sqe_bits: u8,
    pub cqe_bits: u8,
}

const MAX_DATA_TRANSFER_SIZE: usize = 256 * 1024;

const _: () = assert!(
    MAX_DATA_TRANSFER_SIZE.is_power_of_two()
        && MAX_DATA_TRANSFER_SIZE.is_multiple_of(PAGE_SIZE)
        && MAX_DATA_TRANSFER_SIZE / PAGE_SIZE > 1
);
