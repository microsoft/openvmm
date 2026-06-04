// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel VT-d (Virtualization Technology for Directed I/O) IOMMU emulator
//! specification types.
//!
//! This crate defines the spec-derived register layouts, root/context table
//! entries, second-level page table entries, interrupt remapping table entries,
//! and invalidation queue descriptors for the Intel VT-d IOMMU, based on the
//! Intel Virtualization Technology for Directed I/O Architecture Specification,
//! Rev 4.1.

#![forbid(unsafe_code)]

pub mod spec;
