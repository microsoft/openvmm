// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An implementation of an NVMe controller emulator.

#![forbid(unsafe_code)]

pub mod command_match;
mod error;
mod namespace;
mod pci;
mod prp;
mod queue;
pub mod resolver;
mod workers;

#[cfg(test)]
mod tests;

pub use pci::NvmeFaultController;
pub use pci::NvmeFaultControllerCaps;
pub use workers::NvmeFaultControllerClient;

use guestmem::ranges::PagedRange;
use nvme_spec as spec;
use workers::NsidConflict;

// Device configuration shared by PCI and NVMe.
const DOORBELL_STRIDE_BITS: u8 = 2;
const VENDOR_ID: u16 = 0x1414;
const NVME_VERSION: u32 = 0x00020000;
const MAX_QES: u16 = 32;
const BAR0_LEN: u64 = 0x10000;
const IOSQES: u8 = 6;
const IOCQES: u8 = 4;

// NVMe page sizes. This must match the `PagedRange` page size.
const PAGE_SIZE: usize = 4096;
const PAGE_SIZE64: u64 = 4096;
const PAGE_MASK: u64 = !(PAGE_SIZE64 - 1);
const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();
const _: () = assert!(PAGE_SIZE == PagedRange::PAGE_SIZE);
