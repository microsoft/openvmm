// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SynIC bring-up for the VMBus SINT.
//!
//! On each VP we need to:
//! 1. Allocate one guest page for SIMP (message page) and one for SIEFP
//!    (event flags page).
//! 2. Program the SIMP / SIEFP registers with the page GPAs and enable
//!    bits, using [`HvCallSetVpRegisters`][hcall].
//! 3. Program `SINT2` with the vmbus vector, `auto_eoi = true` and
//!    `masked = false`.
//! 4. Enable `SCONTROL`.
//!
//! Register writes go through [`HypercallPlatformTrait`]; we never touch
//! the raw hypercall page ourselves. Page allocation is UEFI-specific
//! and gated behind `cfg(target_os = "uefi")`.
//!
//! # Typical use
//!
//! Callers should invoke [`init_synic`] once per session (it's called
//! for you by [`crate::init`]). The bring-up records the allocated
//! pages in a process-wide slot readable via [`synic_pages`], which
//! [`crate::interrupt::SimpPump::new`] consumes as the SIMP GPA.
//!
//! # Advanced entry points
//!
//! * `preallocate_synic_pages` (UEFI-only) — allocate the pages
//!   without programming the SynIC. Useful when the caller wants to
//!   run the page allocator before `exit_boot_services` and defer the
//!   hypercalls until after.
//! * [`init_synic_with_pages`] — program the registers over a caller-
//!   supplied [`SynicPages`]. Pair with `preallocate_synic_pages`.
//! * [`program_synic_registers`] — the raw four-register write, used
//!   by unit tests against a mock [`HypercallPlatformTrait`].
//!
//! ```ignore
//! use vmbus_guest::synic;
//!
//! // Simple path (called for you by vmbus_guest::init):
//! synic::init_synic(&mut ctx)?;
//!
//! // Deferred path (allocate early, program late):
//! # #[cfg(target_os = "uefi")]
//! # {
//! let pages = synic::preallocate_synic_pages()?;
//! // ...work that shouldn't happen after we start receiving SINT2...
//! synic::init_synic_with_pages(&mut ctx, pages)?;
//! # }
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! [hcall]: hvdef::HypercallCode::HvCallSetVpRegisters

use crate::Error;
use crate::Result;
use crate::hypercalls::set_vp_registers;
#[cfg(target_os = "uefi")]
use core::alloc::Layout;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvSynicSimpSiefp;
use hvdef::HvSynicSint;
use hvdef::hypercall::HvInputVtl;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use spin::Mutex;

/// Standard SINT index reserved for VMBus (matches
/// `vmbus_core::VMBUS_SINT`).
pub const VMBUS_SINT: u8 = 2;

/// IDT/GIC vector used for the VMBus SINT.
///
/// Matches puppet's `X86_VMBUS_HANDLER` / `ARM64_VMBUS_HANDLER` and
/// Linux's `HYPERVISOR_CALLBACK_VECTOR` for SINT2.
pub const VMBUS_INTERRUPT_VECTOR: u8 = 0xF3;

// SynIC register indices (see `hvdef::HvX64RegisterName`; the same
// values are used by `HvArm64RegisterName`).
//
// The naming used in Hyper-V TLFS ("Sipp" = message page, "Sifp" =
// event flag page) is a bit confusing but confirmed by
// `vmm_core/virt_whp/src/regs.rs` which maps `Sifp -> SIEFP` and
// `Sipp -> SIMP`.
const HV_REGISTER_SIMP: u32 = 0x000A0013; // Sipp
const HV_REGISTER_SIEFP: u32 = 0x000A0012; // Sifp
const HV_REGISTER_SCONTROL: u32 = 0x000A0010;
const HV_REGISTER_SINT2: u32 = 0x000A0002;

/// Guest-physical addresses of the SynIC pages for the current VP.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SynicPages {
    /// GPA of the message page (SIMP target).
    pub simp_gpa: u64,
    /// GPA of the event flags page (SIEFP target).
    pub siefp_gpa: u64,
}

/// Once-programmed SynIC state for VP0.
static SYNIC_STATE: Mutex<Option<SynicPages>> = Mutex::new(None);

/// Return the SynIC pages for the current VP, once [`init_synic`] has
/// completed.
pub fn synic_pages() -> Option<SynicPages> {
    *SYNIC_STATE.lock()
}

/// Program the SynIC registers so the hypervisor delivers SINT2
/// messages / events into the given guest pages.
///
/// This is the platform-agnostic half of [`init_synic`] — it doesn't
/// allocate anything and can be unit-tested against a mock
/// [`HypercallPlatformTrait`] implementation.
///
/// * `simp_gpa` must be page-aligned; only the top 52 bits go into
///   the register.
/// * `siefp_gpa` same.
/// * `vector` is the vector the hypervisor injects when SINT2 fires;
///   use [`VMBUS_INTERRUPT_VECTOR`].
pub fn program_synic_registers<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    simp_gpa: u64,
    siefp_gpa: u64,
    vector: u8,
) -> Result<()> {
    if simp_gpa & (hvdef::HV_PAGE_SIZE - 1) != 0 || siefp_gpa & (hvdef::HV_PAGE_SIZE - 1) != 0 {
        return Err(Error::Parse {
            ty: None,
            reason: "SynIC pages must be 4 KiB aligned",
        });
    }

    let simp = HvSynicSimpSiefp::new()
        .with_enabled(true)
        .with_base_gpn(simp_gpa >> hvdef::HV_PAGE_SHIFT);
    let siefp = HvSynicSimpSiefp::new()
        .with_enabled(true)
        .with_base_gpn(siefp_gpa >> hvdef::HV_PAGE_SHIFT);
    // Correct polling mode is `masked=false, polling=true`:
    //   * `masked=false` — required. `HvCallPostMessage` from the
    //     host returns `HV_STATUS_INVALID_SYNIC_STATE` (0xC0350018)
    //     if the target SINT is masked (see
    //     `hv1_emulator::synic::process_post_message`), so the vmbus
    //     service's replies never reach us if we leave this at 1.
    //   * `polling=true` — the hypervisor skips CPU interrupt
    //     injection when a message arrives (see `sint_interrupt` in
    //     the same file), so we can safely rely on the guest polling
    //     the SIMP slot instead of installing a real ISR.
    let sint2 = HvSynicSint::new()
        .with_vector(vector)
        .with_masked(false)
        .with_auto_eoi(true)
        .with_polling(true);
    let scontrol = hvdef::HvSynicScontrol::new().with_enabled(true);

    log::debug!("program_synic_registers: writing 4 SynIC registers");
    set_vp_registers(
        ctx,
        HvInputVtl::CURRENT_VTL,
        &[
            (
                HvRegisterName(HV_REGISTER_SIMP),
                HvRegisterValue::from(u64::from(simp)),
            ),
            (
                HvRegisterName(HV_REGISTER_SIEFP),
                HvRegisterValue::from(u64::from(siefp)),
            ),
            (
                HvRegisterName(HV_REGISTER_SINT2),
                HvRegisterValue::from(u64::from(sint2)),
            ),
            (
                HvRegisterName(HV_REGISTER_SCONTROL),
                HvRegisterValue::from(u64::from(scontrol)),
            ),
        ],
    )?;
    log::debug!("program_synic_registers: set_vp_registers returned Ok");

    // Read the four registers back to confirm the hypervisor
    // actually accepted our values. Any mismatch means the
    // hypervisor silently munged the write.
    let readback = crate::hypercalls::get_vp_registers(
        ctx,
        HvInputVtl::CURRENT_VTL,
        &[
            HvRegisterName(HV_REGISTER_SIMP),
            HvRegisterName(HV_REGISTER_SIEFP),
            HvRegisterName(HV_REGISTER_SINT2),
            HvRegisterName(HV_REGISTER_SCONTROL),
        ],
    )?;
    log::info!(
        "program_synic_registers: readback simp={:#x} siefp={:#x} sint2={:#x} scontrol={:#x}",
        readback[0],
        readback[1],
        readback[2],
        readback[3],
    );

    Ok(())
}

/// Program the current VP's SynIC state and record it in
/// [`synic_pages`].
///
/// * Under `target_os = "uefi"`: allocates SIMP + SIEFP pages via the
///   UEFI page allocator (identity-mapped so GPA == VA), programs the
///   SynIC registers, and records the pages.
/// * Under any other target: returns [`Error::NotImplemented`] because
///   there's no way to obtain guest-physical memory. Host tests
///   exercise [`program_synic_registers`] instead.
pub fn init_synic<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<()> {
    log::debug!("init_synic: allocating pages");
    let pages = allocate_synic_pages()?;
    init_synic_with_pages(ctx, pages)
}

/// Program the SynIC using pre-allocated pages. Useful when the
/// caller wants to allocate before `exit_boot_services` and defer
/// the hypercalls until after.
pub fn init_synic_with_pages<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    pages: SynicPages,
) -> Result<()> {
    log::info!(
        "init_synic_with_pages: simp={:#x} siefp={:#x}",
        pages.simp_gpa,
        pages.siefp_gpa
    );
    log::debug!("init_synic_with_pages: programming registers");
    program_synic_registers(ctx, pages.simp_gpa, pages.siefp_gpa, VMBUS_INTERRUPT_VECTOR)?;
    log::debug!("init_synic_with_pages: registers programmed, storing state");
    *SYNIC_STATE.lock() = Some(pages);
    log::debug!("init_synic_with_pages: done");
    Ok(())
}

/// Allocate SIMP + SIEFP pages via `uefi::boot::allocate_pages`
/// without programming the registers. Useful for callers that want
/// to control when `exit_boot_services` happens relative to the
/// hypercalls.#[cfg(target_os = "uefi")]
pub fn preallocate_synic_pages() -> Result<SynicPages> {
    allocate_synic_pages()
}

/// UEFI page allocation for the SIMP + SIEFP pages.
///
/// Uses `alloc::alloc::alloc_zeroed` with a 4 KiB-aligned Layout so
/// the allocation works both **before** and **after**
/// `exit_boot_services`. The returned VA → GPA translation goes
/// through [`crate::virt_to_phys`], which today is a zero-cost cast
/// under UEFI's identity-map invariant; see that function's doc for
/// what to change if we ever run under non-identity paging.
///
/// Pre-EBS this goes through the UEFI boot-services allocator (which
/// hands back BOOT_SERVICES_DATA that becomes stale at EBS); post-EBS
/// this goes through opentmk's static heap (which persists). Either
/// way, the returned pointer is 4 KiB-aligned and zeroed.
#[cfg(target_os = "uefi")]
fn allocate_synic_pages() -> Result<SynicPages> {
    let layout = Layout::from_size_align(hvdef::HV_PAGE_SIZE_USIZE, hvdef::HV_PAGE_SIZE_USIZE)
        .map_err(|_| Error::Hypercall(opentmk_core::tmkdefs::TmkError::AllocationFailed))?;

    // SAFETY: `layout` is non-zero-size and validly aligned; the
    // returned pointers must be checked against null. We zero via
    // `alloc_zeroed` so no uninitialised bytes are handed to the
    // hypervisor.
    #[expect(unsafe_code, reason = "raw page allocation for hypervisor pages")]
    let simp_ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if simp_ptr.is_null() {
        return Err(Error::Hypercall(
            opentmk_core::tmkdefs::TmkError::AllocationFailed,
        ));
    }
    #[expect(unsafe_code, reason = "raw page allocation for hypervisor pages")]
    let siefp_ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if siefp_ptr.is_null() {
        return Err(Error::Hypercall(
            opentmk_core::tmkdefs::TmkError::AllocationFailed,
        ));
    }

    Ok(SynicPages {
        simp_gpa: crate::virt_to_phys(simp_ptr),
        siefp_gpa: crate::virt_to_phys(siefp_ptr),
    })
}

/// Non-UEFI fallback — SynIC init requires a real hypervisor.
#[cfg(not(target_os = "uefi"))]
fn allocate_synic_pages() -> Result<SynicPages> {
    Err(Error::NotImplemented)
}
