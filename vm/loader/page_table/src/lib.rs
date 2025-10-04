// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables.

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

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

/// A trait for an indexable, mutable, and extendable working memory buffer for page table building
pub trait PageTableBuffer:
    core::ops::Index<usize, Output = Self::Element> + core::ops::IndexMut<usize, Output = Self::Element>
{
    /// Associated Type defining the element type stored in the buffer
    type Element;

    fn new() -> Self;

    fn push(&mut self, item: Self::Element);

    fn extend(&mut self, items: &[Self::Element]);

    fn len(&self) -> usize;

    fn as_slice(&self) -> &[Self::Element];

    fn as_mut_slice(&mut self) -> &mut [Self::Element];

    fn truncate(&mut self, new_len: usize);

    fn iter_mut(&mut self) -> core::slice::IterMut<'_, Self::Element>;
}
