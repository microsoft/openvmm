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
//! Register writes go through
//! [`HypercallTrait`]; we never touch
//! the raw hypercall page ourselves. Page allocation is UEFI-specific
//! and gated behind `cfg(target_os = "uefi")`.
//!
//! [hcall]: hvdef::HypercallCode::HvCallSetVpRegisters

use crate::Error;
use crate::Result;
use crate::hypercalls::set_vp_registers;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvSynicSimpSiefp;
use hvdef::HvSynicSint;
use hvdef::hypercall::HvInputVtl;
use opentmk::context::HypercallTrait;
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
/// [`HypercallTrait`] implementation.
///
/// * `simp_gpa` must be page-aligned; only the top 52 bits go into
///   the register.
/// * `siefp_gpa` same.
/// * `vector` is the vector the hypervisor injects when SINT2 fires;
///   use [`VMBUS_INTERRUPT_VECTOR`].
pub fn program_synic_registers<C: HypercallTrait>(
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
    let sint2 = HvSynicSint::new()
        .with_vector(vector)
        .with_masked(false)
        .with_auto_eoi(true);
    let scontrol = hvdef::HvSynicScontrol::new().with_enabled(true);

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
pub fn init_synic<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    let pages = allocate_synic_pages()?;
    program_synic_registers(ctx, pages.simp_gpa, pages.siefp_gpa, VMBUS_INTERRUPT_VECTOR)?;
    *SYNIC_STATE.lock() = Some(pages);
    Ok(())
}

/// UEFI page allocation for the SIMP + SIEFP pages.
///
/// Uses `AllocateType::AnyPages` in `LOADER_DATA` so the pages survive
/// boot-services exit. Under UEFI, the identity-mapped VA is
/// numerically equal to the GPA, so we cast pointer → u64.
#[cfg(target_os = "uefi")]
fn allocate_synic_pages() -> Result<SynicPages> {
    use uefi::boot::AllocateType;
    use uefi::boot::MemoryType;
    use uefi::boot::allocate_pages;

    let simp = allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|_| Error::Hypercall(opentmk::tmkdefs::TmkError::AllocationFailed))?;
    let siefp = allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|_| Error::Hypercall(opentmk::tmkdefs::TmkError::AllocationFailed))?;

    // Zero the pages before handing them to the hypervisor so we don't
    // leak old boot-services data into the SynIC state.
    //
    // SAFETY: `allocate_pages` returns a valid 4 KiB region we
    // exclusively own; zeroing 4 KiB there is well-defined.
    #[expect(unsafe_code, reason = "raw page zero before publishing to hypervisor")]
    unsafe {
        core::ptr::write_bytes(simp.as_ptr(), 0, hvdef::HV_PAGE_SIZE_USIZE);
        core::ptr::write_bytes(siefp.as_ptr(), 0, hvdef::HV_PAGE_SIZE_USIZE);
    }

    Ok(SynicPages {
        simp_gpa: simp.as_ptr() as u64,
        siefp_gpa: siefp.as_ptr() as u64,
    })
}

/// Non-UEFI fallback — SynIC init requires a real hypervisor.
#[cfg(not(target_os = "uefi"))]
fn allocate_synic_pages() -> Result<SynicPages> {
    Err(Error::NotImplemented)
}
