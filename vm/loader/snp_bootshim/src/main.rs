// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured SEV-SNP boot stage that reads host devices, accepts sparse guest
//! RAM, builds complete ACPI, and enters a direct-boot Linux kernel.
//!
//! The IGVM generator starts this x86_64 bare-metal payload with `RSI` pointing
//! to a measured [`SnpBootShimParams`] page. After validating that complete
//! handoff, the shim makes each omitted RAM range private through the GHCB MSR
//! protocol, executes `PVALIDATE`, repairs its cache state, and jumps to the
//! measured Linux entry point with the architectural direct-boot registers.
//!
//! It has no command-line interface and is only useful as an IGVM component.
//! SNP Linux-direct image generation builds and embeds it; invalid parameters
//! or page-state transitions stop the guest instead of continuing with unsafe
//! memory.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: The bootshim issues PVALIDATE, reads freshly accepted pages, and
// transfers control directly to the measured Linux entrypoint.
#![cfg_attr(minimal_rt, expect(unsafe_code))]
// Keep shared code visible to rust-analyzer in normal host builds even though
// only tests and the minimal-runtime entry point call it.
#![cfg_attr(not(any(minimal_rt, test)), allow(dead_code))]

extern crate alloc;

mod acpi_tables;
mod host_dt;

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
}

/// Validates the measured generator-to-bootshim handoff before using it.
///
/// The IGVM generator measures both this parameter page and the initial RSI
/// that points to it. A validation failure therefore indicates an incompatible
/// or corrupt image, or a generator bug. The runtime stops instead of using
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

/// Checks that the generator wrote the platform page that this shim expects.
fn check_platform(platform: &SnpBootShimPlatformParams) -> bool {
    platform.magic == SNP_BOOT_SHIM_PLATFORM_MAGIC
        && platform.version == SNP_BOOT_SHIM_PLATFORM_VERSION
}

/// Why the shim stopped the guest.
///
/// The shim sends this code in the GHCB termination request, so the host can
/// log it.
#[cfg(minimal_rt)]
#[derive(Copy, Clone)]
#[repr(u8)]
enum Failure {
    Params = 0x01,
    Platform = 0x02,
    Accept = 0x03,
    E820Full = 0x04,
    Heap = 0x05,
    AcpiTooLarge = 0x06,
    Panic = 0x07,
    DeviceTree = 0x10,
    DeviceTreeProperty = 0x11,
    TooManyCpus = 0x12,
    TooManyRamRanges = 0x13,
    TooManyBridges = 0x14,
    DuplicateSegment = 0x15,
    Ram = 0x16,
    Window = 0x17,
}

#[cfg(minimal_rt)]
impl From<host_dt::Error<'_>> for Failure {
    fn from(error: host_dt::Error<'_>) -> Self {
        match error {
            host_dt::Error::Fdt(_) => Self::DeviceTree,
            host_dt::Error::Property(_) => Self::DeviceTreeProperty,
            host_dt::Error::TooManyCpus => Self::TooManyCpus,
            host_dt::Error::TooManyRamRanges => Self::TooManyRamRanges,
            host_dt::Error::TooManyBridges => Self::TooManyBridges,
            host_dt::Error::DuplicateSegment => Self::DuplicateSegment,
            host_dt::Error::Ram => Self::Ram,
            host_dt::Error::Window => Self::Window,
        }
    }
}

/// Terminates the guest with the GHCB MSR protocol and reports `failure`.
///
/// The shim has no #VC handler, so it cannot report errors through port I/O,
/// such as a serial port. Linux uses reason-code set 1, so the shim uses its
/// own set.
#[cfg(minimal_rt)]
fn terminate(failure: Failure) -> ! {
    const REASON_SET: u64 = 0xf;
    let request =
        x86defs::snp::GhcbInfo::SHUTDOWN_REQUEST.0 | (REASON_SET << 12) | ((failure as u64) << 16);
    // SAFETY: GHCB MSR protocol accesses do not require a shared GHCB page.
    unsafe {
        minimal_rt::arch::msr::write_msr(x86defs::X86X_AMD_MSR_GHCB, request);
        core::arch::asm!("rep vmmcall", options(nostack));
    }
    minimal_rt::arch::fault()
}

/// The heap for building the ACPI tables. Allocations outside
/// [`install_platform_acpi`] panic.
#[cfg(minimal_rt)]
#[global_allocator]
// SAFETY: The shim runs only on the BSP.
static ALLOCATOR: minimal_rt::bump_alloc::BumpAllocator =
    unsafe { minimal_rt::bump_alloc::BumpAllocator::new() };

#[cfg(minimal_rt)]
fn install_platform_acpi(
    params: &SnpBootShimParams,
    platform: &SnpBootShimPlatformParams,
    devices: &host_dt::HostDevices,
) {
    let heap = platform
        .heap_gpa
        .checked_add(platform.heap_size)
        .and_then(|end| memory_range::MemoryRange::try_new(platform.heap_gpa..end).ok());
    let Some(heap) = heap else {
        terminate(Failure::Heap)
    };
    // SAFETY: The measured platform page gives this heap region, which the
    // shim accepted with the other omitted RAM. The shim runs with an
    // identity map.
    unsafe { ALLOCATOR.init(heap) };
    let (rsdp, tables) =
        ALLOCATOR.with_alloc(|| acpi_tables::build(devices, platform.acpi_output_gpa));
    // Do not write past the measured output region.
    if tables.len() as u64 > platform.acpi_output_size {
        terminate(Failure::AcpiTooLarge);
    }

    // SAFETY: The measured platform page gives the output and RSDP regions,
    // which are disjoint from the heap that owns these buffers. Publish the
    // RSDP only after all tables exist.
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

/// Returns the parts of the host RAM ranges above the measured RAM.
///
/// The image measures and imports pages only below `measured_ram_end`, so the
/// shim can accept these parts.
fn extra_ram(
    ram: &[acpi::minimal::Ram],
    measured_ram_end: u64,
) -> impl Iterator<Item = memory_range::MemoryRange> + '_ {
    let above =
        memory_range::MemoryRange::new(measured_ram_end..memory_range::MemoryRange::MAX_ADDRESS);
    ram.iter()
        .map(move |r| r.range.intersection(&above))
        .filter(|r| !r.is_empty())
}

/// Adds the host RAM above the measured RAM to the Linux E820 map.
///
/// The measured zero page already describes the measured RAM. Returns `false`
/// if the map is full.
fn add_e820_ram(
    zero_page: &mut loader_defs::linux::boot_params,
    ram: &[acpi::minimal::Ram],
    measured_ram_end: u64,
) -> bool {
    for range in extra_ram(ram, measured_ram_end) {
        let Some(entry) = zero_page
            .e820_map
            .get_mut(usize::from(zero_page.e820_entries))
        else {
            return false;
        };
        *entry = loader_defs::linux::e820entry {
            addr: range.start().into(),
            size: range.len().into(),
            typ: loader_defs::linux::E820_RAM.into(),
        };
        zero_page.e820_entries += 1;
    }
    true
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

    #[repr(C, align(4096))]
    struct PageTable([u64; 512]);

    static mut PML4: PageTable = PageTable([0; 512]);
    static mut PDPT: PageTable = PageTable([0; 512]);

    const _: () = assert!(loader_defs::linux::SNP_BOOT_SHIM_MAX_RAM_END == 512 << 30);

    /// Runs `f` with an identity map of the low 512 GiB in 1-GiB pages.
    ///
    /// The measured page tables map only the low 4 GiB, but `PVALIDATE` takes
    /// a virtual address. The original page tables are restored afterward.
    pub fn with_large_identity_map(c_bit_mask: u64, f: impl FnOnce()) {
        const PRESENT: u64 = 1 << 0;
        const WRITABLE: u64 = 1 << 1;
        const LARGE_PAGE: u64 = 1 << 7;

        let (pml4, pdpt) = (&raw mut PML4, &raw mut PDPT);
        // SAFETY: The shim runs on one thread, and only this function uses
        // these page tables.
        unsafe {
            for (index, entry) in (*pdpt).0.iter_mut().enumerate() {
                *entry = ((index as u64) << 30) | c_bit_mask | LARGE_PAGE | WRITABLE | PRESENT;
            }
            (*pml4).0[0] = pdpt as u64 | c_bit_mask | WRITABLE | PRESENT;
        }

        let old_cr3: u64;
        // SAFETY: Reading CR3 has no side effects.
        unsafe { asm!("mov {}, cr3", out(reg) old_cr3, options(nomem, nostack, preserves_flags)) };
        // Keep the C-bit of the original CR3 so the CPU reads the tables
        // as private memory.
        let new_cr3 = pml4 as u64 | (old_cr3 & c_bit_mask);
        // SAFETY: The new tables identity-map all memory that the shim uses,
        // with the same encryption, so execution continues unchanged.
        unsafe { asm!("mov cr3, {}", in(reg) new_cr3, options(nostack, preserves_flags)) };
        f();
        // SAFETY: These are the original page tables.
        unsafe { asm!("mov cr3, {}", in(reg) old_cr3, options(nostack, preserves_flags)) };
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
        terminate(Failure::Params);
    }

    // SAFETY: The VMSA points RSI at a measured, accepted, identity-mapped
    // parameter page whose layout is validated below before any range is used.
    let params = unsafe { &*(params_gpa as *const SnpBootShimParams) };
    let ranges = match validate_params(params, params_gpa) {
        Ok(ranges) => ranges,
        Err(_) => terminate(Failure::Params),
    };
    // SAFETY: The measured parameter page gives the address of this measured,
    // imported platform page.
    let platform = unsafe { &*(params.platform_gpa as *const SnpBootShimPlatformParams) };
    if !check_platform(platform) {
        terminate(Failure::Platform);
    }
    // SAFETY: The measured platform page gives the location and size of this
    // imported, private device-tree area. Read it before accepting RAM, so
    // that an unsupported device tree stops the shim early.
    let dt = unsafe {
        core::slice::from_raw_parts(platform.dt_gpa as *const u8, platform.dt_size as usize)
    };
    let devices = match host_dt::read(dt, params.ram_end, platform.c_bit_mask) {
        Ok(devices) => devices,
        Err(error) => terminate(error.into()),
    };

    for &range in ranges {
        if arch::accept_range(range).is_err() {
            terminate(Failure::Accept);
        }
    }
    arch::with_large_identity_map(platform.c_bit_mask, || {
        for range in extra_ram(&devices.ram, params.ram_end) {
            let range = SnpBootShimRange {
                start_gpn: range.start() / PAGE_SIZE,
                page_count: range.len() / PAGE_SIZE,
            };
            if arch::accept_range(range).is_err() {
                terminate(Failure::Accept);
            }
        }
    });
    // SAFETY: The measured parameter page gives the address of this measured,
    // imported zero page. Nothing else references it.
    let zero_page =
        unsafe { &mut *(params.linux_zero_page as *mut loader_defs::linux::boot_params) };
    if !add_e820_ram(zero_page, &devices.ram, params.ram_end) {
        terminate(Failure::E820Full);
    }

    install_platform_acpi(params, platform, &devices);

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
    terminate(Failure::Panic)
}

#[cfg(not(minimal_rt))]
fn main() {}

#[cfg(test)]
mod tests {
    use super::*;
    use loader_defs::linux::SNP_BOOT_SHIM_MAX_RANGES;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    #[test]
    fn adds_only_ram_above_the_measured_ram_to_e820() {
        use acpi::minimal::Ram;
        use memory_range::MemoryRange;

        let ram = [
            Ram {
                range: MemoryRange::new(0..0x8000_0000),
                vnode: 0,
            },
            Ram {
                range: MemoryRange::new(0x1_0000_0000..0x2_0000_0000),
                vnode: 1,
            },
        ];
        let mut zero_page = loader_defs::linux::boot_params::new_zeroed();
        zero_page.e820_entries = 1;
        assert!(add_e820_ram(&mut zero_page, &ram, 0x1000_0000));
        let entries = zero_page.e820_map[..usize::from(zero_page.e820_entries)]
            .iter()
            .map(|e| (e.addr.get(), e.size.get(), e.typ.get()))
            .collect::<Vec<_>>();
        assert_eq!(
            entries[1..],
            [
                (0x1000_0000, 0x7000_0000, 1),
                (0x1_0000_0000, 0x1_0000_0000, 1)
            ]
        );

        zero_page.e820_entries = zero_page.e820_map.len() as u8;
        assert!(!add_e820_ram(&mut zero_page, &ram, 0x1000_0000));
    }

    #[test]
    fn checks_platform_magic_and_version() {
        let platform = SnpBootShimPlatformParams {
            magic: SNP_BOOT_SHIM_PLATFORM_MAGIC,
            version: SNP_BOOT_SHIM_PLATFORM_VERSION,
            ..FromZeros::new_zeroed()
        };
        assert!(check_platform(&platform));
        assert!(!check_platform(&SnpBootShimPlatformParams {
            magic: 0,
            ..platform
        }));
        assert!(!check_platform(&SnpBootShimPlatformParams {
            version: SNP_BOOT_SHIM_PLATFORM_VERSION + 1,
            ..platform
        }));
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
    fn rejects_too_many_ranges() {
        let mut params = valid_params();
        params.range_count = (SNP_BOOT_SHIM_MAX_RANGES + 1) as u32;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::TooManyRanges)
        );
    }
}
