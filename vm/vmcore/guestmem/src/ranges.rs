// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types representing contiguous and discontiguous ranges of guest memory.
//!
//! The core types live in [`guestmem_core::ranges`]; this module re-exports
//! them alongside type aliases that fix the memory-backing type parameter to
//! [`crate::GuestMemory`], preserving the historical API surface.

pub use guestmem_core::ranges::AddressRange;
pub use guestmem_core::ranges::PagedRange;
pub use guestmem_core::ranges::PagedRangeRangeIter;
pub use guestmem_core::ranges::PagedRanges;
pub use guestmem_core::ranges::PagedRangesIter;

/// A [`crate::MemoryRead`] implementation for a [`PagedRange`] over
/// [`crate::GuestMemory`].
pub type PagedRangeReader<'a> = guestmem_core::ranges::PagedRangeReader<'a, crate::GuestMemory>;

/// A [`crate::MemoryWrite`] implementation for a [`PagedRange`] over
/// [`crate::GuestMemory`].
pub type PagedRangeWriter<'a> = guestmem_core::ranges::PagedRangeWriter<'a, crate::GuestMemory>;

/// A [`crate::MemoryRead`] implementation for a [`PagedRanges`] over
/// [`crate::GuestMemory`].
pub type PagedRangesReader<'a, T> =
    guestmem_core::ranges::PagedRangesReader<'a, T, crate::GuestMemory>;

/// A [`crate::MemoryWrite`] implementation for a [`PagedRanges`] over
/// [`crate::GuestMemory`].
pub type PagedRangesWriter<'a, T> =
    guestmem_core::ranges::PagedRangesWriter<'a, T, crate::GuestMemory>;
