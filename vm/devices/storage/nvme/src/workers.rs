// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe admin and IO queue workers.

mod admin;
mod coordinator;
mod io;

pub use admin::AddNamespaceError;
pub use coordinator::NvmeControllerClient;
pub use coordinator::NvmeWorkers;

use crate::PAGE_SIZE;
use crate::error::NvmeError;
use crate::spec;
use inspect::Inspect;

#[derive(Debug, Copy, Clone, Inspect, Default)]
pub struct IoQueueEntrySizes {
    pub sqe_bits: u8,
    pub cqe_bits: u8,
}

const MAX_DATA_TRANSFER_SIZE: usize = 256 * 1024;

/// Fails a command whose FUSE field is not 00b with Invalid Field in Command.
///
/// Identify Controller reports FUSES 0, so no fused operation is supported
/// (NVMe Base 2.1 section 3.4.2), and 11b is reserved.
fn refuse_fused(command: &spec::Command) -> Result<(), NvmeError> {
    if command.cdw0.fuse() == 0 {
        return Ok(());
    }
    Err(NvmeError::do_not_retry(
        spec::Status::INVALID_FIELD_IN_COMMAND,
    ))
}

const _: () = assert!(
    MAX_DATA_TRANSFER_SIZE.is_power_of_two()
        && MAX_DATA_TRANSFER_SIZE.is_multiple_of(PAGE_SIZE)
        && MAX_DATA_TRANSFER_SIZE / PAGE_SIZE > 1
);
