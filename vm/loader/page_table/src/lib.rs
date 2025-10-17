// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables.

#![cfg_attr(not(feature = "std"), no_std)]
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

    /// The page table mapping size is not 2MB aligned
    #[error("page table mapping size {0:#x} is not 2MB-aligned")]
    SizeAlignment(u64),

    /// The page table mapping size is not 2MB aligned
    #[error("start_gpa {0:#x} is not 2MB aligned")]
    StartAlignment(u64),

    /// The page table mapping size is greater than 512GB
    #[error("size {0:#x} is larger than 512GB")]
    MappingSize(u64),

    /// The page table mapping size is missing
    #[error("the page table builder was invoked without a mapping size")]
    MissingSize,

    /// The page table builder is generating overlapping mappings
    #[error("the page table builder was invoked without a mapping size")]
    OverlappingMappings,
}

/// Size of the initial identity map
#[derive(Debug, Copy, Clone)]
pub enum IdentityMapSize {
    /// Identity-map the bottom 4GB
    Size4Gb,
    /// Identity-map the bottom 8GB
    Size8Gb,
}
