// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel VT-d specification-derived types.
//!
//! Register layouts, legacy/scalable translation structures, second-level page table
//! entries, interrupt remapping table entries, and invalidation queue
//! descriptors. All definitions are based on the Intel Virtualization
//! Technology for Directed I/O Architecture Specification, Rev 4.1.

pub mod invalidation;
pub mod irte;
pub mod pte;
pub mod registers;
pub mod root_context;
pub mod scalable;
