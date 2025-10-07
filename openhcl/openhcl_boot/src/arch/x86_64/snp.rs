// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SNP support for the bootshim.

use super::address_space::LocalMap;
use core::arch::asm;
use memory_range::MemoryRange;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use x86defs::X86X_AMD_MSR_GHCB;
use x86defs::snp::GhcbInfo;

pub struct Ghcb;

#[derive(Debug)]
pub enum AcceptGpaStatus {
    Success,
    Retry,
}

#[expect(dead_code)] // Printed via Debug in the error case.
#[derive(Debug)]
pub enum AcceptGpaError {
    MemorySecurityViolation {
        error_code: u32,
        carry_flag: u32,
        page_number: u64,
        large_page: bool,
        validate: bool,
    },
    Unknown,
}

impl Ghcb {
    /// # Safety
    ///
    /// Regardless of the content of the GHCB page or MSR, this instruction should not be able
    /// to cause memory safety issues.
    fn sev_vmgexit() {
        // SAFETY: Using the `vmgexit` instruction forces an exit to the hypervisor but doesn't
        // directly change program state.
        unsafe {
            asm! {r#"
            rep vmmcall
            "#
            }
        }
    }

    pub fn change_page_visibility(range: MemoryRange, host_visible: bool) {
        // SAFETY: Always safe to read the GHCB MSR.
        let previous_value = unsafe { read_msr(X86X_AMD_MSR_GHCB) };
        for page_number in range.start_4k_gpn()..range.end_4k_gpn() {
            let extra_data = if host_visible {
                x86defs::snp::GHCB_DATA_PAGE_STATE_SHARED
            } else {
                x86defs::snp::GHCB_DATA_PAGE_STATE_PRIVATE
            };

            let val = (extra_data << 52) | (page_number << 12) | GhcbInfo::PAGE_STATE_CHANGE.0;

            // SAFETY: Writing known good value to the GHCB MSR.
            let val = unsafe {
                write_msr(X86X_AMD_MSR_GHCB, val);
                Self::sev_vmgexit();
                read_msr(X86X_AMD_MSR_GHCB)
            };

            // High 32 bits are status and should be 0 (HV_STATUS_SUCCESS), Low 32 bits should be
            // GHCB_INFO_PAGE_STATE_UPDATED. Assert if otherwise.

            assert!(
                val == GhcbInfo::PAGE_STATE_UPDATED.0,
                "GhcbInfo::PAGE_STATE_UPDATED returned msr value {val}"
            );
        }

        // SAFETY: Restoring previous GHCB value is safe.
        unsafe { write_msr(X86X_AMD_MSR_GHCB, previous_value) };
    }
}

/// Wrapper around the pvalidate assembly instruction.
fn pvalidate(
    page_number: u64,
    va: u64,
    large_page: bool,
    validate: bool,
) -> Result<AcceptGpaStatus, AcceptGpaError> {
    if large_page {
        assert!(va.is_multiple_of(x86defs::X64_LARGE_PAGE_SIZE));
    } else {
        assert!(va.is_multiple_of(hvdef::HV_PAGE_SIZE))
    }

    let validate_page = validate as u32;
    let page_size = large_page as u32;
    let mut error_code: u32;
    let mut carry_flag: u32 = 0;

    // SAFETY: Issuing pvalidate according to specification.
    unsafe {
        asm!(r#"
        pvalidate
        jnc 2f
        inc {carry_flag:e}
        2:
        "#,
        in("rax") va,
        in("ecx") page_size,
        in("edx") validate_page,
        lateout("eax") error_code,
        carry_flag = inout(reg) carry_flag);
    }

    const SEV_SUCCESS: u32 = 0;
    const SEV_FAIL_SIZEMISMATCH: u32 = 6;

    match (error_code, carry_flag) {
        (SEV_SUCCESS, 0) => Ok(AcceptGpaStatus::Success),
        (SEV_FAIL_SIZEMISMATCH, _) => Ok(AcceptGpaStatus::Retry),
        _ => Err(AcceptGpaError::MemorySecurityViolation {
            error_code,
            carry_flag,
            page_number,
            large_page,
            validate,
        }),
    }
}

/// Accepts or unaccepts a specific gpa range. On SNP systems, this corresponds to issuing a
/// pvalidate over the GPA range with the desired value of the validate bit.
pub fn set_page_acceptance(
    local_map: &mut LocalMap<'_>,
    range: MemoryRange,
    validate: bool,
) -> Result<(), AcceptGpaError> {
    let pages_per_large_page = x86defs::X64_LARGE_PAGE_SIZE / hvdef::HV_PAGE_SIZE;
    let mut page_count = range.page_count_4k();
    let mut page_base = range.start_4k_gpn();

    while page_count != 0 {
        // Attempt to validate a large page.
        // Even when pvalidating a large page, the processor only does a 1 byte read. As a result
        // mapping a single page is sufficient.
        let mapping = local_map.map_pages(
            MemoryRange::from_4k_gpn_range(page_base..page_base + 1),
            true,
        );
        if page_base.is_multiple_of(pages_per_large_page) && page_count >= pages_per_large_page {
            let res = pvalidate(page_base, mapping.data.as_ptr() as u64, true, validate)?;
            match res {
                AcceptGpaStatus::Success => {
                    page_count -= pages_per_large_page;
                    page_base += pages_per_large_page;
                    continue;
                }
                AcceptGpaStatus::Retry => (),
            }
        }

        // Attempt to validate a regular sized page.
        let res = pvalidate(page_base, mapping.data.as_ptr() as u64, false, validate)?;
        match res {
            AcceptGpaStatus::Success => {
                page_count -= 1;
                page_base += 1;
            }
            AcceptGpaStatus::Retry => {
                // Cannot retry on a regular sized page.
                return Err(AcceptGpaError::Unknown);
            }
        }
    }

    Ok(())
}
