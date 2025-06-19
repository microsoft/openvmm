// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

//! aarch64 specifics.

pub mod hypercall;
mod memory;
mod vp;
mod vsm;

use crate::host_params::shim_params::ShimParams;
pub use memory::physical_address_bits;
pub use memory::setup_vtl2_memory;
pub use memory::verify_imported_regions_hash;
pub use vp::setup_vtl2_vp;
pub use vsm::get_isolation_type;

pub fn initialize(_: &ShimParams) {}

pub fn uninitialize(_: &ShimParams) {}

// Entry point.
#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    start = sym crate::rt::start,
    relocate = sym minimal_rt::reloc::relocate,
    stack = sym crate::rt::STACK,
    STACK_COOKIE_LO = const (crate::rt::STACK_COOKIE as u16),
    STACK_COOKIE_HI = const ((crate::rt::STACK_COOKIE >> 16) as u16),
    STACK_SIZE = const crate::rt::STACK_SIZE,
}
