// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal support runtime for `openhcl_boot` and other kernel-mode
//! environments.

#![no_std]
// UNSAFETY: Interacting with low level hardware and bootloader primitives.
#![expect(unsafe_code)]
// Allow the allocator api when compiling with `RUSTFLAGS="--cfg nightly"`. This
// is used for some miri tests for testing the bump allocator.
//
// Do not use a normal feature, as that shows errors with rust-analyzer since
// most people are using stable and enable all features. We could remove this
// once the allocator_api feature is stable.
#![cfg_attr(nightly, feature(allocator_api))]

pub mod arch;
pub mod bump_alloc;
pub mod enlightened_panic;
pub mod reftime;
pub mod rt;

pub use minimal_rt_reloc as reloc;
