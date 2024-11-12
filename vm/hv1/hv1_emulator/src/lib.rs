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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cpuid;
pub mod hv;
pub mod hypercall;
pub mod message_queues;
pub mod synic;
pub mod x86;

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
