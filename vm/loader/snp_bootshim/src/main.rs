// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured SEV-SNP boot stage that validates host topology, accepts sparse
//! guest RAM, builds complete ACPI, and enters a direct-boot Linux kernel.
//!
//! The IGVM generator starts this x86_64 bare-metal payload with `RSI` pointing
//! to a measured [`SnpBootShimParams`] page. After validating that complete
//! handoff, the shim makes each omitted RAM range private through the GHCB MSR
//! protocol, executes `PVALIDATE`, repairs its cache state, and jumps to the
//! measured Linux entry point with the architectural direct-boot registers.
//!
//! It has no command-line interface and is only useful as an IGVM component.
//! SNP Linux-direct image generation builds and embeds it; invalid parameters
//! or page-state transitions fault instead of continuing with unsafe memory.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: The bootshim issues PVALIDATE, reads freshly accepted pages, and
// transfers control directly to the measured Linux entrypoint.
#![cfg_attr(minimal_rt, expect(unsafe_code))]
// Keep shared code visible to rust-analyzer in normal host builds even though
// only tests and the minimal-runtime entry point call it.
#![cfg_attr(not(any(minimal_rt, test)), allow(dead_code))]

extern crate alloc;

mod heap;
mod pcie;
mod topology;

use loader_defs::linux::SNP_BOOT_SHIM_ACPI_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_DT_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_HEAP_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_VERSION;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_VERSION;
use loader_defs::linux::SnpBootShimParams;
use loader_defs::linux::SnpBootShimPlatformParams;
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
    EmptyRange,
    RangeOverflow,
    RangeOutsideRam,
    UnorderedRanges,
    ParamsRangeOverlap,
    InvalidPlatform,
    InvalidPlatformRegion,
    PlatformRangeOverlap,
    HeapNotAccepted,
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
    let params_end = params_gpa
        .checked_add(PAGE_SIZE)
        .ok_or(ParamsError::RangeOverflow)?;
    if params_end > params.ram_end {
        return Err(ParamsError::RangeOutsideRam);
    }
    if params.platform_gpa < params_end
        || !params.platform_gpa.is_multiple_of(PAGE_SIZE)
        || params
            .platform_gpa
            .checked_add(PAGE_SIZE)
            .is_none_or(|end| end > params.ram_end)
    {
        return Err(ParamsError::InvalidPlatform);
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
        if start < params.platform_gpa + PAGE_SIZE && params.platform_gpa < end {
            return Err(ParamsError::ParamsRangeOverlap);
        }
        previous_end = end;
    }

    Ok(ranges)
}

fn platform_region(
    start: u64,
    size: u64,
    ram_end: u64,
) -> Result<core::ops::Range<u64>, ParamsError> {
    let end = start.checked_add(size).ok_or(ParamsError::RangeOverflow)?;
    if start == 0
        || size == 0
        || !start.is_multiple_of(PAGE_SIZE)
        || !size.is_multiple_of(PAGE_SIZE)
        || end > ram_end
    {
        return Err(ParamsError::InvalidPlatformRegion);
    }
    Ok(start..end)
}

fn validate_platform(
    params: &SnpBootShimParams,
    params_gpa: u64,
    platform: &SnpBootShimPlatformParams,
) -> Result<(), ParamsError> {
    let ranges = validate_params(params, params_gpa)?;
    if platform.magic != SNP_BOOT_SHIM_PLATFORM_MAGIC
        || platform.version != SNP_BOOT_SHIM_PLATFORM_VERSION
        || platform.size != size_of::<SnpBootShimPlatformParams>() as u32
        || !(1..=loader_defs::linux::SNP_BOOT_SHIM_MAX_CPUS as u32)
            .contains(&platform.expected_cpu_count)
        || platform.reserved != 0
        || platform.reserved2 != 0
        || platform.dt_size != SNP_BOOT_SHIM_DT_SIZE
        || platform.heap_size != SNP_BOOT_SHIM_HEAP_SIZE
        || platform.acpi_output_size != SNP_BOOT_SHIM_ACPI_SIZE
        || !platform.c_bit_mask.is_power_of_two()
        || !(32..52).contains(&platform.c_bit_mask.trailing_zeros())
        || params.ram_end > 1u64 << 32
    {
        return Err(ParamsError::InvalidPlatform);
    }

    let dt = platform_region(platform.dt_gpa, platform.dt_size, params.ram_end)?;
    let heap = platform_region(platform.heap_gpa, platform.heap_size, params.ram_end)?;
    let output = platform_region(
        platform.acpi_output_gpa,
        platform.acpi_output_size,
        params.ram_end,
    )?;
    let rsdp = platform_region(platform.rsdp_gpa, PAGE_SIZE, params.ram_end)?;
    let shim = platform_region(platform.shim_gpa, platform.shim_size, params.ram_end)?;
    let extension = platform_region(params.platform_gpa, PAGE_SIZE, params.ram_end)?;

    // The generator places permanent ACPI below the legacy RSDP, and
    // temporary DT/heap storage after the loaded shim. Keep that ordering
    // explicit so malformed handoffs cannot alias code or published tables.
    if params.linux_zero_page + PAGE_SIZE > output.start
        || output.end > rsdp.start
        || rsdp.end > params.linux_entry
        || params.linux_entry >= shim.start
        || shim.start < 0x10_0000
        || shim.end > params_gpa
        || params_gpa + PAGE_SIZE > extension.start
        || extension.end > dt.start
        || dt.end > heap.start
    {
        return Err(ParamsError::PlatformRangeOverlap);
    }

    let mut heap_accepted = false;
    for range in ranges {
        let start = range.start_gpn * PAGE_SIZE;
        let end = start + range.page_count * PAGE_SIZE;
        let zero_page = params.linux_zero_page..params.linux_zero_page + PAGE_SIZE;
        for protected in [&output, &rsdp, &shim, &extension, &dt, &zero_page] {
            if start < protected.end && protected.start < end {
                return Err(ParamsError::PlatformRangeOverlap);
            }
        }
        if (start..end).contains(&params.linux_entry) {
            return Err(ParamsError::PlatformRangeOverlap);
        }
        heap_accepted |= start <= heap.start && end >= heap.end;
    }
    if !heap_accepted {
        return Err(ParamsError::HeapNotAccepted);
    }
    Ok(())
}

/// Stop using the architected GHCB MSR termination protocol.
///
/// TODO: add a detailed, allocation-free SNP diagnostic channel. The current
/// wire notification is a general termination request, never a silent fallback
/// to stale ACPI. Do not use port I/O here: the shim has no #VC handler.
#[cfg(minimal_rt)]
fn terminate() -> ! {
    // SAFETY: GHCB MSR protocol accesses do not require a shared GHCB page.
    unsafe {
        minimal_rt::arch::msr::write_msr(x86defs::X86X_AMD_MSR_GHCB, 0x100);
        core::arch::asm!("rep vmmcall", options(nostack));
    }
    minimal_rt::arch::fault()
}

#[cfg(minimal_rt)]
fn install_platform_acpi(params: &SnpBootShimParams, platform: &SnpBootShimPlatformParams) {
    // SAFETY: validate_platform checked these disjoint, bounded regions.
    // RAM acceptance completed before heap initialization and any writes.
    let initialized =
        unsafe { heap::HEAP.init(platform.heap_gpa as usize, platform.heap_size as usize) };
    if !initialized {
        terminate();
    }
    // SAFETY: The measured platform descriptor fixes these mapped regions;
    // only the bytes inside the validated DT region are host-controlled.
    let dt = unsafe {
        core::slice::from_raw_parts(platform.dt_gpa as *const u8, platform.dt_size as usize)
    };
    let (rsdp, tables) = match pcie::build_acpi(
        dt,
        platform.expected_cpu_count,
        platform.acpi_output_gpa,
        platform.acpi_output_size as usize,
        params.ram_end,
        platform.c_bit_mask,
    ) {
        Ok(output) => output,
        Err(_) => terminate(),
    };
    if tables.len() > platform.acpi_output_size as usize
        || rsdp.len() != size_of::<acpi_spec::Rsdp>()
    {
        terminate();
    }

    // SAFETY: The output and pinned RSDP regions are disjoint from the heap
    // owning these Vec buffers. Publish discovery only after all tables exist.
    unsafe {
        core::ptr::copy_nonoverlapping(
            tables.as_ptr(),
            platform.acpi_output_gpa as *mut u8,
            tables.len(),
        );
        core::ptr::copy_nonoverlapping(rsdp.as_ptr(), platform.rsdp_gpa as *mut u8, rsdp.len());
        let zero_page = &mut *(params.linux_zero_page as *mut loader_defs::linux::boot_params);
        zero_page.acpi_rsdp_addr = platform.rsdp_gpa;
    }
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
const STACK_SIZE: usize = 32 * 1024;

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
    let platform = {
        // SAFETY: validate_params checked the measured extension page address
        // and bounds. It is imported before the BSP starts, not accepted here.
        let platform = unsafe { &*(params.platform_gpa as *const SnpBootShimPlatformParams) };
        if validate_platform(params, params_gpa, platform).is_err() {
            terminate();
        }
        // SAFETY: The measured descriptor bounds this already-imported private
        // parameter area. Preflight is allocation-free and rejects MMIO/RAM
        // collisions before any omitted RAM (including the heap) is accepted.
        let dt = unsafe {
            core::slice::from_raw_parts(platform.dt_gpa as *const u8, platform.dt_size as usize)
        };
        if pcie::validate_device_tree(
            dt,
            platform.expected_cpu_count,
            params.ram_end,
            platform.c_bit_mask,
        )
        .is_err()
        {
            terminate();
        }
        platform
    };

    for &range in ranges {
        if arch::accept_range(range).is_err() {
            minimal_rt::arch::fault();
        }
    }

    install_platform_acpi(params, platform);

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
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    fn platform_params() -> (SnpBootShimParams, SnpBootShimPlatformParams) {
        let mut params = valid_params();
        params.platform_gpa = 0x203000;
        params.ram_end = 0x800000;
        params.range_count = 1;
        params.ranges[0] = SnpBootShimRange {
            start_gpn: 0x214000 / PAGE_SIZE,
            page_count: SNP_BOOT_SHIM_HEAP_SIZE / PAGE_SIZE,
        };
        let platform = SnpBootShimPlatformParams {
            magic: SNP_BOOT_SHIM_PLATFORM_MAGIC,
            version: SNP_BOOT_SHIM_PLATFORM_VERSION,
            dt_gpa: 0x204000,
            dt_size: SNP_BOOT_SHIM_DT_SIZE,
            heap_gpa: 0x214000,
            heap_size: SNP_BOOT_SHIM_HEAP_SIZE,
            size: size_of::<SnpBootShimPlatformParams>() as u32,
            expected_cpu_count: 2,
            acpi_output_gpa: 0x1a000,
            acpi_output_size: SNP_BOOT_SHIM_ACPI_SIZE,
            rsdp_gpa: 0xe0000,
            shim_gpa: 0x200000,
            shim_size: 2 * PAGE_SIZE,
            c_bit_mask: 1 << 51,
            ..FromZeros::new_zeroed()
        };
        (params, platform)
    }

    #[test]
    fn validates_platform_handoff_and_accepted_heap() {
        let (mut params, platform) = platform_params();
        validate_platform(&params, 0x202000, &platform).unwrap();
        params.range_count = 0;
        assert_eq!(
            validate_platform(&params, 0x202000, &platform),
            Err(ParamsError::HeapNotAccepted)
        );
    }

    #[test]
    fn rejects_unsafe_platform_regions() {
        let (params, platform) = platform_params();
        let changes: &[fn(&mut SnpBootShimPlatformParams)] = &[
            |p| p.magic = 0,
            |p| p.version += 1,
            |p| p.reserved = 1,
            |p| p.dt_gpa += 1,
            |p| p.dt_size += PAGE_SIZE,
            |p| p.heap_gpa = p.dt_gpa,
            |p| p.heap_size += PAGE_SIZE,
            |p| p.acpi_output_gpa = 0x1000,
            |p| p.size += 1,
            |p| p.expected_cpu_count = 0,
            |p| p.expected_cpu_count = 256,
            |p| p.rsdp_gpa = p.acpi_output_gpa,
            |p| p.shim_gpa = 0x100000,
            |p| p.shim_size = u64::MAX,
            |p| p.c_bit_mask = 1 << 31,
            |p| p.c_bit_mask = 3 << 50,
        ];
        for change in changes {
            let mut bad = platform;
            change(&mut bad);
            assert!(
                validate_platform(&params, 0x202000, &bad).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_accepting_imported_platform_pages() {
        let (mut params, platform) = platform_params();
        params.range_count = 2;
        params.ranges[1] = params.ranges[0];
        for start in [
            params.platform_gpa,
            platform.dt_gpa,
            platform.acpi_output_gpa,
        ] {
            params.ranges[0] = SnpBootShimRange {
                start_gpn: start / PAGE_SIZE,
                page_count: 1,
            };
            let expected = if start == params.platform_gpa {
                ParamsError::ParamsRangeOverlap
            } else {
                ParamsError::PlatformRangeOverlap
            };
            assert_eq!(
                validate_platform(&params, 0x202000, &platform),
                Err(expected)
            );
        }
    }

    fn valid_params() -> SnpBootShimParams {
        SnpBootShimParams {
            magic: SNP_BOOT_SHIM_PARAMS_MAGIC,
            version: SNP_BOOT_SHIM_PARAMS_VERSION,
            range_count: 2,
            linux_entry: 0x10_0000,
            linux_zero_page: 0x2000,
            ram_end: 0x20_0000,
            platform_gpa: 0x3000,
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

        for version in [1, 2, SNP_BOOT_SHIM_PARAMS_VERSION + 1] {
            params = valid_params();
            params.version = version;
            assert_eq!(
                validate_params(&params, 0x1000),
                Err(ParamsError::UnsupportedVersion)
            );
        }
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
        params.platform_gpa = 1;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidPlatform)
        );
    }
}
