// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured SEV-SNP boot stage that reads host devices, builds complete
//! ACPI, and boots Linux through the kernel's own EFI stub.
//!
//! The IGVM generator starts this x86_64 bare-metal payload with `RSI` pointing
//! to a measured [`SnpBootShimParams`] page. After validating that complete
//! handoff, the shim:
//!
//! 1. reads the CPUs, RAM, PCIe host bridges, and COM ports from the host
//!    device tree;
//! 2. builds a UEFI memory map, in which the RAM that the image did not
//!    import is unaccepted;
//! 3. accepts only the memory that it and the EFI stub need, through the GHCB
//!    MSR protocol and `PVALIDATE`;
//! 4. builds the ACPI tables;
//! 5. installs a `#VC` handler that answers `CPUID`; and
//! 6. enters the kernel's PE/COFF entry point with a minimal set of UEFI boot
//!    services.
//!
//! The EFI stub then does the work that Linux expects from firmware. It also
//! sets up lazy acceptance, so Linux accepts the remaining memory on demand.
//!
//! It has no command-line interface and is only useful as an IGVM component.
//! SNP Linux-direct image generation builds and embeds it; invalid parameters
//! or page-state transitions stop the guest instead of continuing with unsafe
//! memory.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: The bootshim issues PVALIDATE, installs exception handlers,
// implements UEFI services for the EFI stub, and transfers control to the
// measured kernel.
#![cfg_attr(minimal_rt, expect(unsafe_code))]
// Keep shared code visible to rust-analyzer in normal host builds even though
// only tests and the minimal-runtime entry point call it.
#![cfg_attr(not(any(minimal_rt, test)), allow(dead_code))]

extern crate alloc;

mod acpi_tables;
mod efi;
mod host_dt;
mod memory_map;
mod vc;

use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_VERSION;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_FLAG_ACCEPT_ALL_MEMORY;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_VERSION;
use loader_defs::linux::SnpBootShimParams;
use loader_defs::linux::SnpBootShimPlatformParams;
use loader_defs::linux::SnpBootShimRange;
use memory_range::MemoryRange;

const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Eq, PartialEq)]
enum ParamsError {
    UnalignedParams,
    InvalidMagic,
    UnsupportedVersion,
    TooManyRanges,
    InvalidRamEnd,
    Reserved,
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
    if params.reserved != [0; 2] {
        return Err(ParamsError::Reserved);
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

#[derive(Debug, Eq, PartialEq)]
enum PlatformError {
    InvalidMagic,
    UnsupportedVersion,
    UnknownFlags,
    InvalidCBit,
    /// A region is missing, unaligned, overflows, or lies outside RAM.
    InvalidRegion(&'static str),
    /// A work area does not lie in one omitted RAM range.
    NotOmitted(&'static str),
    InvalidKernelEntry,
    InvalidStack,
    InvalidCommandLine,
}

/// UEFI spec 2.3.4 gives an x64 image at least 128 KiB of stack.
const MIN_EFI_STACK_SIZE: u64 = 128 * 1024;

/// Validates the measured platform page.
///
/// Like the parameter page, the generator measures this page, so a failure
/// indicates an incompatible or corrupt image.
fn validate_platform(
    platform: &SnpBootShimPlatformParams,
    ram_end: u64,
    omitted: &[SnpBootShimRange],
) -> Result<(), PlatformError> {
    if platform.magic != SNP_BOOT_SHIM_PLATFORM_MAGIC {
        return Err(PlatformError::InvalidMagic);
    }
    if platform.version != SNP_BOOT_SHIM_PLATFORM_VERSION {
        return Err(PlatformError::UnsupportedVersion);
    }
    if platform.flags & !SNP_BOOT_SHIM_PLATFORM_FLAG_ACCEPT_ALL_MEMORY != 0 {
        return Err(PlatformError::UnknownFlags);
    }
    if !platform.c_bit_mask.is_power_of_two() {
        return Err(PlatformError::InvalidCBit);
    }

    let region = |name, gpa: u64, size: u64| {
        size.checked_next_multiple_of(PAGE_SIZE)
            .and_then(|size| gpa.checked_add(size))
            .filter(|&end| size != 0 && gpa.is_multiple_of(PAGE_SIZE) && end <= ram_end)
            .map(|end| MemoryRange::new(gpa..end))
            .ok_or(PlatformError::InvalidRegion(name))
    };
    let p = platform;
    region("device tree", p.dt_gpa, p.dt_size)?;
    region("ACPI output", p.acpi_output_gpa, p.acpi_output_size)?;
    region("RSDP", p.rsdp_gpa, PAGE_SIZE)?;
    region("SMBIOS", p.smbios_gpa, p.smbios_size)?;
    region("secrets page", p.secrets_gpa, PAGE_SIZE)?;
    region("CPUID page", p.cpuid_gpa, PAGE_SIZE)?;
    region("CC blob", p.cc_blob_gpa, PAGE_SIZE)?;
    region("command line", p.cmdline_gpa, p.cmdline_size)?;
    if p.initrd_size != 0 {
        region("initrd", p.initrd_gpa, p.initrd_size)?;
    }
    let kernel = region("kernel", p.kernel_gpa, p.kernel_size)?;
    if !kernel.contains_addr(p.kernel_entry) {
        return Err(PlatformError::InvalidKernelEntry);
    }
    // The command line is UTF-16 and ends with a NUL character.
    if p.cmdline_size < 2 || !p.cmdline_size.is_multiple_of(2) || p.cmdline_size > u32::MAX.into() {
        return Err(PlatformError::InvalidCommandLine);
    }
    if p.stack_size < MIN_EFI_STACK_SIZE {
        return Err(PlatformError::InvalidStack);
    }

    for (name, gpa, size) in [
        ("heap", p.heap_gpa, p.heap_size),
        ("stack", p.stack_gpa, p.stack_size),
        ("boot pool", p.boot_pool_gpa, p.boot_pool_size),
    ] {
        let range = region(name, gpa, size)?;
        if !omitted.iter().any(|omitted| {
            let start = omitted.start_gpn * PAGE_SIZE;
            start <= range.start() && range.end() <= start + omitted.page_count * PAGE_SIZE
        }) {
            return Err(PlatformError::NotOmitted(name));
        }
    }
    Ok(())
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
    MemoryMap = 0x04,
    Heap = 0x05,
    AcpiTooLarge = 0x06,
    Panic = 0x07,
    /// The EFI stub returned, which means that it failed.
    EfiStub = 0x08,
    /// The EFI stub called `Exit`.
    EfiExit = 0x09,
    /// A `#VC` exception for something other than `CPUID`.
    UnhandledVc = 0x0a,
    /// An unexpected exception.
    Exception = 0x0b,
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
fn install_platform_acpi(platform: &SnpBootShimPlatformParams, devices: &host_dt::HostDevices) {
    let heap = platform
        .heap_gpa
        .checked_add(platform.heap_size)
        .and_then(|end| MemoryRange::try_new(platform.heap_gpa..end).ok());
    let Some(heap) = heap else {
        terminate(Failure::Heap)
    };
    // SAFETY: The measured platform page gives this heap region, which the
    // shim accepted. The shim runs with an identity map.
    unsafe { ALLOCATOR.init(heap) };
    let (rsdp, tables) =
        ALLOCATOR.with_alloc(|| acpi_tables::build(devices, platform.acpi_output_gpa));
    // Do not write past the measured output regions.
    if tables.len() as u64 > platform.acpi_output_size || rsdp.len() as u64 > PAGE_SIZE {
        terminate(Failure::AcpiTooLarge);
    }

    // SAFETY: The measured platform page gives the output and RSDP regions,
    // which are disjoint from the heap that owns these buffers. The EFI
    // configuration table publishes the RSDP after all tables exist.
    unsafe {
        core::ptr::copy_nonoverlapping(
            tables.as_ptr(),
            platform.acpi_output_gpa as *mut u8,
            tables.len(),
        );
        core::ptr::copy_nonoverlapping(rsdp.as_ptr(), platform.rsdp_gpa as *mut u8, rsdp.len());
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

    #[repr(C, align(4096))]
    struct PageTable([u64; 512]);

    static mut PML4: PageTable = PageTable([0; 512]);
    static mut PDPT: PageTable = PageTable([0; 512]);

    const _: () = assert!(loader_defs::linux::SNP_BOOT_SHIM_MAX_RAM_END == 512 << 30);

    /// Switches to an identity map of the low 512 GiB in 1-GiB pages.
    ///
    /// The measured page tables map only the low 4 GiB, but `PVALIDATE` takes
    /// a virtual address, and the EFI stub uses memory anywhere in RAM. These
    /// page tables stay in use until Linux installs its own. They lie in the
    /// shim's image, which the memory map reports as runtime-services memory.
    pub fn install_large_identity_map(c_bit_mask: u64) {
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
const STACK_SIZE: usize = 64 * 1024;

#[cfg(minimal_rt)]
#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);

#[cfg(minimal_rt)]
static mut STACK: Stack = Stack([0; STACK_SIZE]);

/// Returns the shim's own image, from its ELF header to the end of its BSS.
#[cfg(minimal_rt)]
fn shim_range() -> MemoryRange {
    unsafe extern "C" {
        static __ehdr_start: u8;
        static _end: u8;
    }
    let start = (&raw const __ehdr_start) as u64 & !(PAGE_SIZE - 1);
    let end = ((&raw const _end) as u64).next_multiple_of(PAGE_SIZE);
    MemoryRange::new(start..end)
}

#[cfg(minimal_rt)]
fn accept(range: MemoryRange) -> bool {
    arch::accept_range(SnpBootShimRange {
        start_gpn: range.start() / PAGE_SIZE,
        page_count: range.len() / PAGE_SIZE,
    })
    .is_ok()
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
    if validate_platform(platform, params.ram_end, ranges).is_err() {
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

    arch::install_large_identity_map(platform.c_bit_mask);
    let layout = efi::ImageLayout {
        ram: &devices.ram,
        measured_ram_end: params.ram_end,
        omitted: ranges,
        platform,
        shim: shim_range(),
    };
    let Ok(mut map) = efi::build_memory_map(&layout) else {
        terminate(Failure::MemoryMap)
    };
    let accept_all = platform.flags & SNP_BOOT_SHIM_PLATFORM_FLAG_ACCEPT_ALL_MEMORY != 0;
    let result = efi::initial_acceptance(&layout)
        .try_for_each(|(range, ty)| map.accept(range, ty, &mut accept))
        .and_then(|()| {
            if accept_all {
                map.accept_all(&mut accept)
            } else {
                Ok(())
            }
        });
    match result {
        Ok(()) => {}
        Err(memory_map::Error::Accept) => terminate(Failure::Accept),
        Err(_) => terminate(Failure::MemoryMap),
    }

    install_platform_acpi(platform, &devices);
    vc::install(platform.cpuid_gpa);

    let p = platform;
    efi::enter_linux(
        map,
        &efi::Boot {
            kernel: MemoryRange::new(p.kernel_gpa..p.kernel_gpa + p.kernel_size),
            kernel_entry: p.kernel_entry,
            initrd: (p.initrd_size != 0).then(|| {
                MemoryRange::new(
                    p.initrd_gpa..p.initrd_gpa + p.initrd_size.next_multiple_of(PAGE_SIZE),
                )
            }),
            initrd_size: p.initrd_size,
            cmdline_gpa: p.cmdline_gpa,
            cmdline_size: p.cmdline_size,
            rsdp_gpa: p.rsdp_gpa,
            smbios_gpa: p.smbios_gpa,
            cc_blob_gpa: p.cc_blob_gpa,
            stack_top: (p.stack_gpa + p.stack_size) & !0xf,
        },
    )
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

    const MB: u64 = 1 << 20;

    fn valid_params() -> SnpBootShimParams {
        SnpBootShimParams {
            magic: SNP_BOOT_SHIM_PARAMS_MAGIC,
            version: SNP_BOOT_SHIM_PARAMS_VERSION,
            range_count: 2,
            ram_end: 0x20_0000,
            platform_gpa: 0x3000,
            reserved: [0; 2],
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
    fn rejects_bad_magic_version_and_reserved() {
        let mut params = valid_params();
        params.magic = 0;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::InvalidMagic)
        );

        for version in [1, 3, SNP_BOOT_SHIM_PARAMS_VERSION + 1] {
            params = valid_params();
            params.version = version;
            assert_eq!(
                validate_params(&params, 0x1000),
                Err(ParamsError::UnsupportedVersion)
            );
        }

        params = valid_params();
        params.reserved[1] = 1;
        assert_eq!(validate_params(&params, 0x1000), Err(ParamsError::Reserved));
    }

    #[test]
    fn rejects_unaligned_parameter_page_and_ram_end() {
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
    fn rejects_too_many_ranges() {
        let mut params = valid_params();
        params.range_count = (SNP_BOOT_SHIM_MAX_RANGES + 1) as u32;
        assert_eq!(
            validate_params(&params, 0x1000),
            Err(ParamsError::TooManyRanges)
        );
    }

    const RAM_END: u64 = 64 * MB;

    /// One omitted range holds the heap, the stack, and the boot pool.
    fn omitted() -> [SnpBootShimRange; 1] {
        [SnpBootShimRange {
            start_gpn: (8 * MB) / PAGE_SIZE,
            page_count: (56 * MB) / PAGE_SIZE,
        }]
    }

    fn valid_platform() -> SnpBootShimPlatformParams {
        SnpBootShimPlatformParams {
            magic: SNP_BOOT_SHIM_PLATFORM_MAGIC,
            version: SNP_BOOT_SHIM_PLATFORM_VERSION,
            c_bit_mask: 1 << 51,
            secrets_gpa: 0x10000,
            cpuid_gpa: 0x11000,
            cc_blob_gpa: 0x12000,
            smbios_gpa: 0x13000,
            smbios_size: 0x200,
            rsdp_gpa: 0x14000,
            acpi_output_gpa: 0x15000,
            acpi_output_size: 0x10000,
            kernel_gpa: MB,
            kernel_size: 4 * MB,
            kernel_entry: MB + 0x1000,
            initrd_gpa: 5 * MB,
            initrd_size: 0x1234,
            cmdline_gpa: 6 * MB,
            cmdline_size: 30,
            dt_gpa: 7 * MB,
            dt_size: 0x10000,
            heap_gpa: 8 * MB,
            heap_size: MB,
            stack_gpa: 9 * MB,
            stack_size: MIN_EFI_STACK_SIZE,
            boot_pool_gpa: 16 * MB,
            boot_pool_size: 32 * MB,
            ..FromZeros::new_zeroed()
        }
    }

    #[test]
    fn accepts_a_valid_platform() {
        validate_platform(&valid_platform(), RAM_END, &omitted()).unwrap();
        // The initrd is optional.
        let platform = SnpBootShimPlatformParams {
            initrd_gpa: 0,
            initrd_size: 0,
            ..valid_platform()
        };
        validate_platform(&platform, RAM_END, &omitted()).unwrap();
    }

    #[test]
    fn rejects_bad_platform_header() {
        for (platform, error) in [
            (
                SnpBootShimPlatformParams {
                    magic: 0,
                    ..valid_platform()
                },
                PlatformError::InvalidMagic,
            ),
            (
                SnpBootShimPlatformParams {
                    version: SNP_BOOT_SHIM_PLATFORM_VERSION - 1,
                    ..valid_platform()
                },
                PlatformError::UnsupportedVersion,
            ),
            (
                SnpBootShimPlatformParams {
                    flags: 1 << 31,
                    ..valid_platform()
                },
                PlatformError::UnknownFlags,
            ),
            (
                SnpBootShimPlatformParams {
                    c_bit_mask: 3 << 50,
                    ..valid_platform()
                },
                PlatformError::InvalidCBit,
            ),
        ] {
            assert_eq!(
                validate_platform(&platform, RAM_END, &omitted()),
                Err(error)
            );
        }
    }

    #[test]
    fn rejects_bad_platform_regions() {
        for (platform, error) in [
            (
                SnpBootShimPlatformParams {
                    kernel_size: 0,
                    ..valid_platform()
                },
                PlatformError::InvalidRegion("kernel"),
            ),
            (
                SnpBootShimPlatformParams {
                    smbios_gpa: 0x13001,
                    ..valid_platform()
                },
                PlatformError::InvalidRegion("SMBIOS"),
            ),
            (
                SnpBootShimPlatformParams {
                    initrd_gpa: RAM_END,
                    ..valid_platform()
                },
                PlatformError::InvalidRegion("initrd"),
            ),
            (
                SnpBootShimPlatformParams {
                    boot_pool_gpa: !(PAGE_SIZE - 1),
                    ..valid_platform()
                },
                PlatformError::InvalidRegion("boot pool"),
            ),
            (
                SnpBootShimPlatformParams {
                    kernel_entry: 5 * MB,
                    ..valid_platform()
                },
                PlatformError::InvalidKernelEntry,
            ),
            (
                SnpBootShimPlatformParams {
                    cmdline_size: 31,
                    ..valid_platform()
                },
                PlatformError::InvalidCommandLine,
            ),
            (
                SnpBootShimPlatformParams {
                    stack_size: MIN_EFI_STACK_SIZE - PAGE_SIZE,
                    ..valid_platform()
                },
                PlatformError::InvalidStack,
            ),
            (
                SnpBootShimPlatformParams {
                    heap_gpa: 7 * MB,
                    ..valid_platform()
                },
                PlatformError::NotOmitted("heap"),
            ),
        ] {
            assert_eq!(
                validate_platform(&platform, RAM_END, &omitted()),
                Err(error)
            );
        }
    }
}
