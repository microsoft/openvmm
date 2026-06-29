// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AMD IOMMU specification-derived types.
//!
//! Register layouts, device table entries, page table entries, command/event
//! formats, and interrupt remapping table entries. All definitions are based on
//! the AMD I/O Virtualization Technology (IOMMU) Specification, Rev 3.11.

pub mod commands;
pub mod dte;
pub mod events;
pub mod irte;
pub mod pte;
pub mod registers;
