// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables.

pub mod aarch64;
pub mod x64;

/// Size of the initial identity map
#[derive(Debug, Copy, Clone)]
pub enum IdentityMapSize {
    /// Identity-map the bottom 4GB
    Size4Gb,
    /// Identity-map the bottom 8GB
    Size8Gb,
}
