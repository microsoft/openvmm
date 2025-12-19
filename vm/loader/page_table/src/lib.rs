// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables.

#![cfg_attr(not(any(feature = "std", test)), no_std)]
//TODO docs are missing on pub page table functions for aarch64
#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod aarch64;
pub mod x64;

use thiserror::Error;

/// Errors returned by the Page Table Builder
#[derive(Debug, PartialEq, Eq, Error)]
pub enum Error {
    /// The PageTableBuilder bytes buffer does not match the size of the struct buffer
    #[error(
        "PageTableBuilder bytes buffer size {bytes_buf} does not match the struct buffer size [{struct_buf}]"
    )]
    BadBufferSize { bytes_buf: usize, struct_buf: usize },

    /// The constructed page tables are larger than the amount memory given for construction by the caller
    #[error(
        "constructed page tables are larger than the amount memory given for construction by the caller"
    )]
    NotEnoughMemory,

    /// The page table builder mapping ranges are not sorted
    #[error("the page table builder was invoked with unsorted mapping ranges")]
    UnsortedMappings,

    /// The page table builder was given an invalid range
    #[error("page table builder range.end() < range.start()")]
    InvalidRange,

    /// The page table builder is generating overlapping mappings
    #[error("the page table builder was invoked with overlapping mappings")]
    OverlappingMappings,

    /// The page table builder tried to overwrite a leaf mapping
    #[error("the page table builder attempted to overwite a leaf mapping")]
    AttemptedEntryOverwrite,
}

/// Size of the initial identity map
#[derive(Debug, Copy, Clone)]
pub enum IdentityMapSize {
    /// Identity-map the bottom 4GB
    Size4Gb,
    /// Identity-map the bottom 8GB
    Size8Gb,
}
