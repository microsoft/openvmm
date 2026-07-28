// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SynIC bring-up for the VMBus SINT.
//!
//! On each VP we need to:
//! 1. Allocate one guest page for SIMP (message page) and one for SIEFP
//!    (event flags page).
//! 2. Program `HV_X64_MSR_SIMP` / `HV_X64_MSR_SIEFP` with the page GPAs
//!    and enable bits.
//! 3. Program `HV_X64_MSR_SINT2` with the vmbus vector, `auto_eoi=true`
//!    and `masked=false`.
//! 4. Enable `HV_X64_MSR_SCONTROL`.
//!
//! All register accesses go through
//! [`HypercallTrait`]; we never touch
//! the raw hypercall page.

use crate::Error;
use crate::Result;
use opentmk::context::HypercallTrait;

/// Standard SINT index reserved for VMBus (matches
/// `vmbus_core::VMBUS_SINT`).
pub const VMBUS_SINT: u8 = 2;

/// IDT/GIC vector used for the VMBus SINT.
///
/// Matches puppet's `X86_VMBUS_HANDLER` / `ARM64_VMBUS_HANDLER` and Linux's
/// `HYPERVISOR_CALLBACK_VECTOR`.
pub const VMBUS_INTERRUPT_VECTOR: u8 = 0xF3;

/// Page-aligned pair of guest pages used for the SynIC message / event
/// pages of the current VP.
pub struct SynicPages {
    /// GPA of the message page (SIMP target).
    pub simp_gpa: u64,
    /// GPA of the event flags page (SIEFP target).
    pub siefp_gpa: u64,
}

/// Program the current VP's SynIC state and install the message-page /
/// event-page for VMBus use.
///
/// See §4 steps 2–3 of `tasks/vmbus-port-design.md`.
///
/// **Not implemented in the scaffold** — the follow-up patch wires up the
/// UEFI page allocator and the `HvCallSetVpRegisters` writes.
pub fn init_synic<C: HypercallTrait>(_ctx: &mut C) -> Result<()> {
    // TODO(vmbus-port): allocate SIMP/SIEFP pages via the UEFI page
    // allocator (see §4 step 2 of the design doc), program the SynIC
    // MSRs through `HvCallSetVpRegisters`, and install the interrupt
    // handler for `VMBUS_INTERRUPT_VECTOR`. Track state in a
    // `Once<SynicPages>` so re-entry is a no-op.
    Err(Error::NotImplemented)
}

/// Return the SynIC pages for the current VP, once [`init_synic`] has
/// completed.
pub fn synic_pages() -> Option<&'static SynicPages> {
    None
}
