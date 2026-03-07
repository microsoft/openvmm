// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe driver implementation.

#![forbid(unsafe_code)]

mod driver;
mod namespace;
mod queue_pair;
mod queues;
mod registers;
#[cfg(test)]
mod tests;

pub use self::driver::NvmeDriver;
pub use self::driver::save_restore;
pub use self::namespace::NamespaceError;
pub use self::namespace::NamespaceHandle;
pub use self::queue_pair::RequestError;

use nvme_spec as spec;

const NVME_PAGE_SHIFT: u8 = 12;
