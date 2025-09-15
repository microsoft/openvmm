// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables.

#![no_std]
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
        "PageTableBuilder bytes buffer size [{bytes_buf}] does not match the struct buffer size [{struct_buf}]"
    )]
    BadBufferSize { bytes_buf: usize, struct_buf: usize },
}

/// Size of the initial identity map
#[derive(Debug, Copy, Clone)]
pub enum IdentityMapSize {
    /// Identity-map the bottom 4GB
    Size4Gb,
    /// Identity-map the bottom 8GB
    Size8Gb,
}
