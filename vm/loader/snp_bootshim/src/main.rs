// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Accepts sparse SNP IGVM RAM before entering a direct-boot Linux kernel.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: The bootshim issues PVALIDATE, reads freshly accepted pages, and
// transfers control directly to the measured Linux entrypoint.
#![cfg_attr(minimal_rt, expect(unsafe_code))]
// Keep shared code visible to rust-analyzer in normal host builds even though
// only tests and the minimal-runtime entry point call it.
#![cfg_attr(not(any(minimal_rt, test)), allow(dead_code))]

use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_VERSION;
use loader_defs::linux::SnpBootShimParams;
use loader_defs::linux::SnpBootShimRange;

const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Eq, PartialEq)]
enum ParamsError {
    UnalignedParams,
    InvalidMagic,
    UnsupportedVersion,
    TooManyRanges,
    InvalidRamEnd,
    InvalidLinuxEntry,
    InvalidLinuxZeroPage,
    ReservedNotZero,
    EmptyRange,
    RangeOverflow,
    RangeOutsideRam,
    UnorderedRanges,
    ParamsRangeOverlap,
}

/// Validates the measured generator-to-bootshim handoff before using it.
///
/// The IGVM generator measures both this parameter page and the initial RSI
/// that points to it. A validation failure therefore indicates an incompatible
/// or corrupt image, or a generator bug. The runtime faults instead of using
/// an invalid address or accepting an invalid RAM range.
fn validate_params(
    params: &SnpBootShimParams,
    params_gpa: u64,
) -> Result<&[SnpBootShimRange], ParamsError> {
    if !params_gpa.is_multiple_of(PAGE_SIZE) {
        return Err(ParamsError::UnalignedParams);
    }
    if params.magic != SNP_BOOT_SHIM_PARAMS_MAGIC {
        return Err(ParamsError::InvalidMagic);
    }
    if params.version != SNP_BOOT_SHIM_PARAMS_VERSION {
        return Err(ParamsError::UnsupportedVersion);
    }
    let range_count =
        usize::try_from(params.range_count).map_err(|_| ParamsError::TooManyRanges)?;
    let ranges = params
        .ranges
        .get(..range_count)
        .ok_or(ParamsError::TooManyRanges)?;
    if params.ram_end == 0 || !params.ram_end.is_multiple_of(PAGE_SIZE) {
        return Err(ParamsError::InvalidRamEnd);
    }
    if params.reserved != 0 {
        return Err(ParamsError::ReservedNotZero);
    }
    let params_end = params_gpa
        .checked_add(PAGE_SIZE)
        .ok_or(ParamsError::RangeOverflow)?;
    if params_end > params.ram_end {
        return Err(ParamsError::RangeOutsideRam);
    }
    if params.linux_entry == 0
        || params.linux_entry >= params.ram_end
        || (params_gpa..params_end).contains(&params.linux_entry)
    {
        return Err(ParamsError::InvalidLinuxEntry);
    }
    let linux_zero_page_end = params
        .linux_zero_page
        .checked_add(PAGE_SIZE)
        .ok_or(ParamsError::RangeOverflow)?;
    if params.linux_zero_page == 0
        || !params.linux_zero_page.is_multiple_of(PAGE_SIZE)
        || linux_zero_page_end > params.ram_end
        || params.linux_zero_page < params_end && params_gpa < linux_zero_page_end
    {
        return Err(ParamsError::InvalidLinuxZeroPage);
    }

    let mut previous_end = 0;
    for range in ranges {
        if range.page_count == 0 {
            return Err(ParamsError::EmptyRange);
        }
        let start = range
            .start_gpn
            .checked_mul(PAGE_SIZE)
            .ok_or(ParamsError::RangeOverflow)?;
        let len = range
            .page_count
            .checked_mul(PAGE_SIZE)
            .ok_or(ParamsError::RangeOverflow)?;
        let end = start.checked_add(len).ok_or(ParamsError::RangeOverflow)?;
        if end > params.ram_end {
            return Err(ParamsError::RangeOutsideRam);
        }
        if start < previous_end {
            return Err(ParamsError::UnorderedRanges);
        }
        if start < params_end && params_gpa < end {
            return Err(ParamsError::ParamsRangeOverlap);
        }
        previous_end = end;
    }

    Ok(ranges)
}

#[cfg(minimal_rt)]
mod arch {
    use super::PAGE_SIZE;
    use core::arch::asm;
    use loader_defs::linux::SnpBootShimRange;
    use minimal_rt::arch::msr::read_msr;
    use minimal_rt::arch::msr::write_msr;
    use x86defs::X86X_AMD_MSR_GHCB;
    use x86defs::snp::GHCB_DATA_PAGE_STATE_LARGE_PAGE;
    use x86defs::snp::GHCB_DATA_PAGE_STATE_PRIVATE;
    use x86defs::snp::GhcbInfo;
    use x86defs::snp::GhcbMsr;

    const LARGE_PAGE_SIZE: u64 = x86defs::X64_LARGE_PAGE_SIZE;

    enum PvalidateStatus {
        Success,
        SizeMismatch,
    }

    #[derive(Debug)]
    pub struct AcceptError;

    fn set_page_private(page_base: u64, large_page: bool) -> Result<(), AcceptError> {
        let extra_data = GHCB_DATA_PAGE_STATE_PRIVATE
            | if large_page {
                GHCB_DATA_PAGE_STATE_LARGE_PAGE
            } else {
                0
            };
        let request = GhcbMsr::new()
            .with_info(GhcbInfo::PAGE_STATE_CHANGE.0)
            .with_pfn(page_base)
            .with_extra_data(extra_data);
        let response = GhcbMsr::from_bits(
            // SAFETY: The request uses the architected GHCB MSR page-state
            // change protocol to assign measured guest RAM as private.
            unsafe {
                write_msr(X86X_AMD_MSR_GHCB, request.into_bits());
                asm!("rep vmmcall", options(nostack));
                read_msr(X86X_AMD_MSR_GHCB)
            },
        );
        if response.into_bits() == GhcbInfo::PAGE_STATE_UPDATED.0 {
            Ok(())
        } else {
            Err(AcceptError)
        }
    }

    fn pvalidate(va: u64, large_page: bool) -> Result<PvalidateStatus, AcceptError> {
        let page_size = large_page as u32;
        let mut error_code: u32;
        let mut carry_flag: u32 = 0;

        // SAFETY: The bootshim invokes PVALIDATE only on identity-mapped private
        // RAM ranges supplied by its measured parameter page.
        unsafe {
            asm!(
                r#"
                pvalidate
                jnc 2f
                inc {carry_flag:e}
                2:
                "#,
                in("rax") va,
                in("ecx") page_size,
                in("edx") 1u32,
                lateout("eax") error_code,
                carry_flag = inout(reg) carry_flag,
            );
        }

        match (error_code, carry_flag) {
            (0, 0) => Ok(PvalidateStatus::Success),
            (6, _) => Ok(PvalidateStatus::SizeMismatch),
            _ => Err(AcceptError),
        }
    }

    fn fixup_page_cache_state(va: u64) {
        const CACHE_LINE_SIZE: u64 = 64;

        // PVALIDATE can leave stale cache lines from the previous page state.
        // Flush every line before any later consumer uses the accepted page.
        for addr in (va..va + PAGE_SIZE).step_by(CACHE_LINE_SIZE as usize) {
            // SAFETY: `va` is an accepted, identity-mapped page, and `addr`
            // stays within that page.
            unsafe {
                asm!(
                    "clflush [{addr}]",
                    addr = in(reg) addr,
                    options(nostack),
                );
            }
        }
        // Ensure every CLFLUSH completes before this page is reused.
        // SAFETY: MFENCE has no memory operands.
        unsafe {
            asm!("mfence", options(nostack, preserves_flags));
        }
    }

    fn fixup_range_cache_state(start: u64, page_count: u64) {
        for page in 0..page_count {
            fixup_page_cache_state(start + page * PAGE_SIZE);
        }
    }

    pub fn accept_range(range: SnpBootShimRange) -> Result<(), AcceptError> {
        let pages_per_large_page = LARGE_PAGE_SIZE / PAGE_SIZE;
        let mut page_base = range.start_gpn;
        let mut pages_remaining = range.page_count;

        while pages_remaining != 0 {
            if page_base.is_multiple_of(pages_per_large_page)
                && pages_remaining >= pages_per_large_page
                && set_page_private(page_base, true).is_ok()
            {
                let va = page_base * PAGE_SIZE;
                match pvalidate(va, true)? {
                    PvalidateStatus::Success => {
                        fixup_range_cache_state(va, pages_per_large_page);
                        page_base += pages_per_large_page;
                        pages_remaining -= pages_per_large_page;
                        continue;
                    }
                    PvalidateStatus::SizeMismatch => {}
                }
            }

            // Fall back to 4-KiB acceptance when a 2-MiB PVALIDATE is not
            // supported for this range.
            let va = page_base * PAGE_SIZE;
            set_page_private(page_base, false)?;
            match pvalidate(va, false)? {
                PvalidateStatus::Success => {
                    fixup_range_cache_state(va, 1);
                    page_base += 1;
                    pages_remaining -= 1;
                }
                PvalidateStatus::SizeMismatch => return Err(AcceptError),
            }
        }

        Ok(())
    }
}

#[cfg(minimal_rt)]
const STACK_SIZE: usize = 16 * 1024;

#[cfg(minimal_rt)]
#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);

#[cfg(minimal_rt)]
static mut STACK: Stack = Stack([0; STACK_SIZE]);

// Assembly needs fixed symbols for the final indirect handoff. These start in
// zeroed BSS and `start` writes the validated parameter values before use.
#[cfg(minimal_rt)]
static mut LINUX_ENTRY: u64 = 0;

#[cfg(minimal_rt)]
static mut LINUX_ZERO_PAGE: u64 = 0;

#[cfg(minimal_rt)]
fn jump_to_linux() -> ! {
    // SAFETY: The caller accepted all omitted RAM and populated these
    // single-threaded handoff statics from the measured parameter page.
    unsafe {
        core::arch::asm!(
            // Clear registers to restore the original direct-boot register state.
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor edi, edi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            // Stage the architectural initial RFLAGS value after all XORs,
            // since XOR changes the arithmetic flags.
            "push 2",
            // Restore RFLAGS without leaving the staged value on the stack.
            "popfq",
            // Restore RSI to the measured Linux boot-parameter page.
            "mov rsi, qword ptr [rip + {linux_zero_page}]",
            // Explicitly ensure that string operations increment addresses.
            "cld",
            // Use MOV rather than XOR after POPFQ so RFLAGS stays at its
            // architectural direct-boot value.
            "mov rsp, 0",
            // Jump through memory so the Linux entrypoint consumes no GPR.
            "jmp qword ptr [rip + {linux_entry}]",
            linux_entry = sym LINUX_ENTRY,
            linux_zero_page = sym LINUX_ZERO_PAGE,
            options(noreturn),
        )
    }
}

#[cfg(minimal_rt)]
extern "C" fn start(params_gpa: u64) -> ! {
    if !params_gpa.is_multiple_of(PAGE_SIZE) {
        minimal_rt::arch::fault();
    }

    // SAFETY: The VMSA points RSI at a measured, accepted, identity-mapped
    // parameter page whose layout is validated below before any range is used.
    let params = unsafe { &*(params_gpa as *const SnpBootShimParams) };
    let ranges = match validate_params(params, params_gpa) {
        Ok(ranges) => ranges,
        Err(_) => minimal_rt::arch::fault(),
    };

    for &range in ranges {
        if arch::accept_range(range).is_err() {
            minimal_rt::arch::fault();
        }
    }

    // SAFETY: These statics are single-threaded bootshim handoff state. Their
    // values came from the measured parameter page and are consumed
    // immediately by `jump_to_linux`.
    unsafe {
        core::ptr::write_volatile(&raw mut LINUX_ENTRY, params.linux_entry);
        core::ptr::write_volatile(&raw mut LINUX_ZERO_PAGE, params.linux_zero_page);
    }
    jump_to_linux()
}

#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    relocate = sym minimal_rt::reloc::relocate,
    start = sym start,
    stack = sym STACK,
    STACK_SIZE = const STACK_SIZE,
}

#[cfg(minimal_rt)]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    minimal_rt::arch::fault()
}

#[cfg(not(minimal_rt))]
fn main() {}

#[cfg(test)]
mod tests {
    use super::*;
    use loader_defs::linux::SNP_BOOT_SHIM_MAX_RANGES;

    fn valid_params() -> SnpBootShimParams {
        SnpBootShimParams {
            magic: SNP_BOOT_SHIM_PARAMS_MAGIC,
            version: SNP_BOOT_SHIM_PARAMS_VERSION,
            range_count: 2,
            linux_entry: 0x10_0000,
            linux_zero_page: 0x2000,
            ram_end: 0x20_0000,
            reserved: 0,
            ranges: {
                let mut ranges = [SnpBootShimRange {
                    start_gpn: 0,
                    page_count: 0,
                }; SNP_BOOT_SHIM_MAX_RANGES];
                ranges[0] = SnpBootShimRange {
                    start_gpn: 4,
                    page_count: 3,
                };
                ranges[1] = SnpBootShimRange {
                    start_gpn: 8,
                    page_count: 2,
                };
                ranges
            },
        }
    }

    #[test]
    fn validates_empty_ranges() {
        let mut params = valid_params();
        params.range_count = 0;
        assert_eq!(validate_params(&params, 0x1000).unwrap(), []);
    }

    #[test]
    fn validates_ordered_multiple_ranges() {
        let params = valid_params();
        assert_eq!(
            validate_params(&params, 0x1000).unwrap(),
            &params.ranges[..2]
        );
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut params = valid_params();
        params.magic = 0;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidMagic)
        );

        params = valid_params();
        params.version += 1;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::UnsupportedVersion)
        );
    }

    #[test]
    fn rejects_unaligned_parameter_page_and_zero_page() {
        let params = valid_params();
        assert_eq!(
            validate_params(&params, 0x1001),
            Err(ParamsError::UnalignedParams)
        );

        let mut params = valid_params();
        params.ram_end += 1;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidRamEnd)
        );

        params = valid_params();
        params.linux_zero_page += 1;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxZeroPage)
        );
    }

    #[test]
    fn rejects_overflowing_ranges() {
        let mut params = valid_params();
        params.ranges[0] = SnpBootShimRange {
            start_gpn: u64::MAX / PAGE_SIZE + 1,
            page_count: 1,
        };
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::RangeOverflow)
        );

        params = valid_params();
        params.ranges[0] = SnpBootShimRange {
            start_gpn: 1,
            page_count: u64::MAX / PAGE_SIZE + 1,
        };
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::RangeOverflow)
        );
    }

    #[test]
    fn rejects_unordered_and_overlapping_ranges() {
        let mut params = valid_params();
        params.ranges[1].start_gpn = 3;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::UnorderedRanges)
        );

        params = valid_params();
        params.ranges[1].start_gpn = 6;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::UnorderedRanges)
        );
    }

    #[test]
    fn rejects_parameter_page_overlap() {
        let mut params = valid_params();
        params.ranges[0] = SnpBootShimRange {
            start_gpn: 1,
            page_count: 1,
        };
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::ParamsRangeOverlap)
        );
    }

    #[test]
    fn rejects_out_of_ram_ranges() {
        let mut params = valid_params();
        params.ranges[1] = SnpBootShimRange {
            start_gpn: params.ram_end / PAGE_SIZE,
            page_count: 1,
        };
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::RangeOutsideRam)
        );
    }

    #[test]
    fn rejects_invalid_linux_entry_and_zero_page() {
        let mut params = valid_params();
        params.linux_entry = 0;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxEntry)
        );

        params = valid_params();
        params.linux_entry = params.ram_end;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxEntry)
        );

        params = valid_params();
        params.linux_entry = 0x1000;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxEntry)
        );

        params = valid_params();
        params.linux_zero_page = 0;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxZeroPage)
        );

        params = valid_params();
        params.linux_zero_page = params.ram_end;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxZeroPage)
        );

        params = valid_params();
        params.linux_zero_page = 0x1000;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidLinuxZeroPage)
        );
    }

    #[test]
    fn rejects_too_many_ranges_and_nonzero_reserved_data() {
        let mut params = valid_params();
        params.range_count = (SNP_BOOT_SHIM_MAX_RANGES + 1) as u32;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::TooManyRanges)
        );

        params = valid_params();
        params.reserved = 1;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::ReservedNotZero)
        );
    }
}
