// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "x86_64")]

//! x86_64 architecture-specific implementations.

mod address_space;
pub mod hypercall;
mod memory;
pub mod snp;
pub mod tdx;
mod vp;
mod vsm;

use crate::host_params::shim_params::IsolationType;
pub use memory::reserve_pages_for_multivp;
pub use memory::setup_vtl2_memory;
pub use memory::verify_imported_regions_hash;
use safe_intrinsics::cpuid;
pub use vp::setup_vtl2_vp;
pub use vsm::get_isolation_type;
use x86defs::cpuid::CpuidFunction;

pub fn physical_address_bits(isolation: IsolationType) -> u8 {
    if isolation.is_hardware_isolated() {
        unimplemented!("can't trust host cpuid");
    }
    const DEFAULT_PHYSICAL_ADDRESS_SIZE: u8 = 32;

    let max_extended = {
        let result = cpuid(CpuidFunction::ExtendedMaxFunction.0, 0);
        result.eax
    };
    if max_extended >= CpuidFunction::ExtendedAddressSpaceSizes.0 {
        let result = cpuid(CpuidFunction::ExtendedAddressSpaceSizes.0, 0);
        (result.eax & 0xFF) as u8
    } else {
        DEFAULT_PHYSICAL_ADDRESS_SIZE
    }
}

// Entry point.
#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    relocate = sym minimal_rt::reloc::relocate,
    start = sym crate::rt::start,
    stack = sym crate::rt::STACK,
    STACK_COOKIE = const crate::rt::STACK_COOKIE,
    STACK_SIZE = const crate::rt::STACK_SIZE,
}
