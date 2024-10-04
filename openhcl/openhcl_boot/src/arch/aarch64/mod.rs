// Copyright (C) Microsoft Corporation. All rights reserved.

#![cfg(target_arch = "aarch64")]

//! aarch64 specifics.

pub mod hypercall;
mod memory;
mod vp;
mod vsm;

pub use memory::physical_address_bits;
pub use memory::setup_vtl2_memory;
pub use memory::verify_imported_regions_hash;
pub use vp::setup_vtl2_vp;
pub use vsm::get_isolation_type;

// Entry point.
#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    start = sym crate::rt::start,
    relocate = sym minimal_rt::reloc::relocate,
    stack = sym crate::rt::STACK,
}
