// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
    STACK_COOKIE_LO = const (crate::rt::STACK_COOKIE as u16),
    STACK_COOKIE_HI = const ((crate::rt::STACK_COOKIE >> 16) as u16),
    STACK_SIZE = const crate::rt::STACK_SIZE,
    HV_REGISTER_OSID = const hvdef::HvArm64RegisterName::GuestOsId.0,
    OHCL_LOADER_OSID = const hvdef::hypercall::HvGuestOsMicrosoft::new()
        .with_os_id(1)
        .into_bits(),
    HV_REGISTER_GUEST_CRASH_P0 = const hvdef::HvArm64RegisterName::GuestCrashP0.0,
    HV_REGISTER_GUEST_CRASH_P1 = const hvdef::HvArm64RegisterName::GuestCrashP1.0,
    HV_REGISTER_GUEST_CRASH_P2 = const hvdef::HvArm64RegisterName::GuestCrashP2.0,
    HV_REGISTER_GUEST_CRASH_P3 = const hvdef::HvArm64RegisterName::GuestCrashP3.0,
    HV_REGISTER_GUEST_CRASH_P4 = const hvdef::HvArm64RegisterName::GuestCrashP4.0,
    HV_REGISTER_GUEST_CRASH_CTRL = const hvdef::HvArm64RegisterName::GuestCrashCtl.0,
    GUEST_CRASH_CTRL = const hvdef::GuestCrashCtl::new()
        .with_no_crash_dump(true)
        .with_crash_notify(true).into_bits()
}
