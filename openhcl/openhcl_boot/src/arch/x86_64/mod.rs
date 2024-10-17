// Copyright (C) Microsoft Corporation. All rights reserved.

#![cfg(target_arch = "x86_64")]

//! x86_64 architecture-specific implementations.

mod address_space;
pub mod hypercall;
mod memory;
pub mod snp;
pub mod tdx;
mod vp;
mod vsm;

pub use memory::setup_vtl2_memory;
pub use memory::verify_imported_regions_hash;
pub use vp::setup_vtl2_vp;
pub use vsm::get_isolation_type;

use crate::rt::STACK_COOKIE;
use crate::rt::STACK_SIZE;

pub fn physical_address_bits() -> u8 {
    unimplemented!("physical_address_bits not implemented on x64")
}

// Entry point.
#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    relocate = sym minimal_rt::reloc::relocate,
    start = sym crate::rt::start,
    stack = sym crate::rt::STACK,
    STACK_COOKIE = const STACK_COOKIE,
    STACK_SIZE = const STACK_SIZE,
}
