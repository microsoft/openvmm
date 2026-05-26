// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe controller emulator (NVMe 2.0, NVM command set).
//!
//! This crate emulates an NVMe controller as a PCI device with MMIO BAR0,
//! MSI-X, and admin + I/O queue pairs. It targets the
//! [NVMe Base 2.0](https://nvmexpress.org/specifications/) specification
//! (version register reports 0x00020000) with vendor ID 0x1414 (Microsoft).
//!
//! # Architecture
//!
//! - **PCI layer** ([`NvmeController`]) — MMIO BAR0 register handling, PCI
//!   config space, MSI-X interrupt routing, doorbell writes.
//! - **Coordinator** — manages enable/reset sequencing, namespace add/remove.
//! - **Admin worker** — processes admin commands: Identify Controller/Namespace,
//!   Create/Delete I/O Queue, Get/Set Features, Async Event Request.
//! - **I/O workers** — pool of tasks (one per completion queue) processing NVM
//!   commands: READ, WRITE, FLUSH, Dataset Management (TRIM), and persistent
//!   reservation commands.
//!
//! # What it doesn't implement
//!
//! Firmware update, admin-level namespace management (create/delete), multi-path
//! I/O, end-to-end data protection (PI), and save/restore (`SaveRestore`
//! returns not-supported).
//!
//! # Namespace management
//!
//! Namespaces can be added and removed at runtime via [`NvmeControllerClient`].
//! Each namespace wraps a [`Disk`](disk_backend::Disk) and a background task
//! monitors capacity changes via `wait_resize`, completing Async Event Requests
//! with `CHANGED_NAMESPACE_LIST` when the disk size changes.
//!
//! # Key constants
//!
//! - `MAX_DATA_TRANSFER_SIZE`: 256 KB
//! - `MAX_QES`: 256 queue entries
//! - `BAR0_LEN`: 64 KB

#![forbid(unsafe_code)]

mod error;
mod namespace;
mod pci;
mod prp;
mod queue;
pub mod resolver;
mod workers;

#[cfg(test)]
mod tests;

pub use pci::NvmeController;
pub use pci::NvmeControllerCaps;
pub use workers::NsidConflict;
pub use workers::NvmeControllerClient;

use guestmem::ranges::PagedRange;
use nvme_spec as spec;

// Device configuration shared by PCI and NVMe.
const DOORBELL_STRIDE_BITS: u8 = 2;
const VENDOR_ID: u16 = 0x1414;
const NVME_VERSION: u32 = 0x00020000;
const MAX_QES: u16 = 256;
const BAR0_LEN: u64 = 0x10000;
const IOSQES: u8 = 6;
const IOCQES: u8 = 4;

// NVMe page sizes. This must match the `PagedRange` page size.
const PAGE_SIZE: usize = 4096;
const PAGE_SIZE64: u64 = 4096;
const PAGE_MASK: u64 = !(PAGE_SIZE64 - 1);
const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();
const _: () = assert!(PAGE_SIZE == PagedRange::PAGE_SIZE);
