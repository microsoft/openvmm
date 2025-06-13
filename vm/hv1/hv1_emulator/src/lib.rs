// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V hypervisor interface emulator.
//!
//! This crate implements support for emulating the hypervisor's synthetic
//! interrupt controller, synthetic MSRs, and synthetic cpuid leaves, as defined
//! in the [Hypervisor Top Level Functional Specification][].
//!
//! See also the peer crate `hv1_hypercall` for emulating HV#1 hypercalls.
//!
//! [Hypervisor Top Level Functional Specification]:
//!     <https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/tlfs>

pub mod cpuid;
pub mod hv;
pub mod hypercall;
pub mod message_queues;
mod pages;
pub mod synic;
pub mod x86;

pub use pages::LockedOverlayPage;

/// Trait for requesting an interrupt.
pub trait RequestInterrupt {
    /// Requests an interrupt with the specified vector.
    ///
    /// If `auto_eoi` is true, then the APIC should not set ISR when the
    /// interrupt is delivered.
    fn request_interrupt(&mut self, vector: u32, auto_eoi: bool);
}

impl<T: FnMut(u32, bool)> RequestInterrupt for T {
    fn request_interrupt(&mut self, vector: u32, auto_eoi: bool) {
        self(vector, auto_eoi)
    }
}

/// A trait for managing the vtl protections on pages.
pub trait VtlProtectAccess {
    /// Check if the given GPN has the specified VTL permissions, optionally
    /// modify them, then lock them.
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        check_perms: hvdef::HvMapGpaFlags,
        new_perms: Option<hvdef::HvMapGpaFlags>,
    ) -> Result<LockedOverlayPage, hvdef::HvError>;

    /// Unlocks an overlay page by its GPN, restoring its previous permissions.
    fn unlock_overlay_page(&mut self, gpn: u64) -> Result<(), hvdef::HvError>;
}
