// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe driver implementation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod driver;
mod namespace;
mod page_allocator;
mod queue_pair;
mod queues;
mod registers;
#[cfg(test)]
mod tests;

pub use self::driver::save_restore::Error;
pub use self::driver::save_restore::NvmeDriverSavedState;
pub use self::driver::NvmeDriver;
pub use self::namespace::Namespace;
pub use self::namespace::NamespaceError;
pub use self::queue_pair::RequestError;

use nvme_spec as spec;

const NVME_PAGE_SHIFT: u8 = 12;
const NVME_TIMEOUT_INCREMENT: u64 = 500;
const NVME_TIMEOUT_MINIMUM: u64 = 10000;
pub const NVME_TIMEOUT_FUZZER: u64 = 10;
