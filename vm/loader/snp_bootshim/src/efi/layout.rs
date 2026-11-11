// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The UEFI memory map that the shim builds before it calls the EFI stub,
//! and the memory that it accepts first.

use crate::memory_map::Error;
use crate::memory_map::MemoryMap;
use acpi::minimal::Ram;
use loader_defs::linux::SnpBootShimPlatformParams;
use loader_defs::linux::SnpBootShimRange;
use memory_range::MemoryRange;
use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
use uefi_specs::uefi::boot::EfiMemoryType;

/// The shim accepts the omitted RAM below this address before it calls the
/// stub, as firmware does. Linux expects low memory to be usable early.
const LOW_MEMORY_END: u64 = 1 << 20;

/// The regions of the measured image.
pub(crate) struct ImageLayout<'a> {
    /// The host RAM.
    pub ram: &'a [Ram],
    /// The end of the RAM that the image measures.
    pub measured_ram_end: u64,
    /// The RAM that the image does not import.
    pub omitted: &'a [SnpBootShimRange],
    pub platform: &'a SnpBootShimPlatformParams,
    /// The shim's own image, including its BSS.
    pub shim: MemoryRange,
}

fn region(gpa: u64, size: u64) -> MemoryRange {
    MemoryRange::new(gpa..gpa + size.next_multiple_of(EFI_PAGE_SIZE))
}

fn omitted_range(range: &SnpBootShimRange) -> MemoryRange {
    MemoryRange::new(
        range.start_gpn * EFI_PAGE_SIZE..(range.start_gpn + range.page_count) * EFI_PAGE_SIZE,
    )
}

/// Builds the memory map before the shim accepts anything.
///
/// Imported memory is boot-services data unless it has a lasting role:
///
/// * The shim is runtime-services code. It holds the EFI tables, the
///   runtime services, and the exception handlers that run until Linux loads
///   its own IDT.
/// * The kernel image is loader code.
/// * SMBIOS is runtime-services data, which Linux maps encrypted.
/// * The ACPI tables and the RSDP are ACPI reclaim memory.
/// * The SNP secrets, CPUID, and CC-blob pages are reserved.
///
/// Omitted RAM and host RAM above the measured RAM are unaccepted.
pub(crate) fn build_memory_map(layout: &ImageLayout<'_>) -> Result<MemoryMap, Error> {
    let measured = MemoryRange::new(0..layout.measured_ram_end);
    if !covers(layout.ram, measured) {
        return Err(Error::InvalidParameter);
    }

    let mut map = MemoryMap::default();
    let above = MemoryRange::new(layout.measured_ram_end..MemoryRange::MAX_ADDRESS);
    for ram in layout.ram {
        map.set(
            ram.range.intersection(&measured),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        )?;
        map.set(
            ram.range.intersection(&above),
            EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
        )?;
    }
    let p = layout.platform;
    for (range, ty) in [
        (layout.shim, EfiMemoryType::EFI_RUNTIME_SERVICES_CODE),
        (
            region(p.kernel_gpa, p.kernel_size),
            EfiMemoryType::EFI_LOADER_CODE,
        ),
        (
            region(p.smbios_gpa, p.smbios_size),
            EfiMemoryType::EFI_RUNTIME_SERVICES_DATA,
        ),
        (
            region(p.acpi_output_gpa, p.acpi_output_size),
            EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        ),
        (
            region(p.rsdp_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        ),
        (
            region(p.secrets_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
        (
            region(p.cpuid_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
        (
            region(p.cc_blob_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
    ] {
        map.set(range, ty)?;
    }
    // Apply these last. A typed region can contain omitted pages, such as a
    // hole between the shim's ELF segments, and those pages stay unaccepted.
    for range in layout.omitted {
        map.set(
            omitted_range(range),
            EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
        )?;
    }
    Ok(map)
}

/// Returns `true` if `ram` covers every page of `range`.
fn covers(ram: &[Ram], range: MemoryRange) -> bool {
    let mut cursor = range.start();
    while cursor < range.end() {
        let Some(next) = ram
            .iter()
            .find(|ram| ram.range.contains_addr(cursor))
            .map(|ram| ram.range.end())
        else {
            return false;
        };
        cursor = next;
    }
    true
}

/// Returns the memory that the shim accepts before it calls the stub, and the
/// type of each part.
///
/// This is the ACPI heap, the EFI stack, the boot pool for the decompressed
/// kernel, and the omitted RAM below 1 MiB. All other omitted memory stays
/// unaccepted until the stub or Linux needs it.
pub(crate) fn initial_acceptance<'a>(
    layout: &'a ImageLayout<'a>,
) -> impl Iterator<Item = (MemoryRange, EfiMemoryType)> + 'a {
    let p = layout.platform;
    let low = MemoryRange::new(0..LOW_MEMORY_END);
    [
        (
            region(p.heap_gpa, p.heap_size),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        ),
        (
            region(p.stack_gpa, p.stack_size),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        ),
        (
            region(p.boot_pool_gpa, p.boot_pool_size),
            EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
        ),
    ]
    .into_iter()
    .chain(layout.omitted.iter().filter_map(move |range| {
        let range = omitted_range(range).intersection(&low);
        (!range.is_empty()).then_some((range, EfiMemoryType::EFI_CONVENTIONAL_MEMORY))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    const MB: u64 = 1 << 20;

    fn test_platform() -> SnpBootShimPlatformParams {
        SnpBootShimPlatformParams {
            secrets_gpa: 0x10000,
            cpuid_gpa: 0x11000,
            cc_blob_gpa: 0x12000,
            smbios_gpa: 0x13000,
            smbios_size: 0x1000,
            rsdp_gpa: 0x14000,
            acpi_output_gpa: 0x15000,
            acpi_output_size: 0x10000,
            kernel_gpa: MB,
            kernel_size: 4 * MB,
            heap_gpa: 6 * MB,
            heap_size: MB,
            stack_gpa: 7 * MB,
            stack_size: 0x40000,
            boot_pool_gpa: 16 * MB,
            boot_pool_size: 16 * MB,
            ..FromZeros::new_zeroed()
        }
    }

    fn range(start: u64, end: u64) -> SnpBootShimRange {
        SnpBootShimRange {
            start_gpn: start / EFI_PAGE_SIZE,
            page_count: (end - start) / EFI_PAGE_SIZE,
        }
    }

    fn types(map: &MemoryMap) -> Vec<(u64, u64, EfiMemoryType)> {
        map.descriptors()
            .map(|d| {
                (
                    d.physical_start,
                    d.physical_start + d.number_of_pages * EFI_PAGE_SIZE,
                    d.typ,
                )
            })
            .collect()
    }

    #[test]
    fn builds_typed_map_and_accepts_work_areas() {
        let platform = test_platform();
        let ram = [
            Ram {
                range: MemoryRange::new(0..64 * MB),
                vnode: 0,
            },
            Ram {
                range: MemoryRange::new(4096 * MB..4160 * MB),
                vnode: 0,
            },
        ];
        // The second range is a hole between the shim's ELF segments.
        let omitted = [
            range(0, 0x10000),
            range(5 * MB + 0x1000, 5 * MB + 0x2000),
            range(6 * MB, 48 * MB),
        ];
        let layout = ImageLayout {
            ram: &ram,
            measured_ram_end: 48 * MB,
            omitted: &omitted,
            platform: &platform,
            shim: MemoryRange::new(5 * MB..6 * MB),
        };
        let mut map = build_memory_map(&layout).unwrap();
        let mut accepted = Vec::new();
        for (range, ty) in initial_acceptance(&layout) {
            map.accept(range, ty, &mut |range| {
                accepted.push(range);
                true
            })
            .unwrap();
        }

        use EfiMemoryType as T;
        assert_eq!(
            types(&map),
            [
                (0, 0x10000, T::EFI_CONVENTIONAL_MEMORY),
                (0x10000, 0x13000, T::EFI_RESERVED_MEMORY_TYPE),
                (0x13000, 0x14000, T::EFI_RUNTIME_SERVICES_DATA),
                (0x14000, 0x25000, T::EFI_ACPI_RECLAIM_MEMORY),
                (0x25000, MB, T::EFI_BOOT_SERVICES_DATA),
                (MB, 5 * MB, T::EFI_LOADER_CODE),
                (5 * MB, 5 * MB + 0x1000, T::EFI_RUNTIME_SERVICES_CODE),
                (
                    5 * MB + 0x1000,
                    5 * MB + 0x2000,
                    T::EFI_UNACCEPTED_MEMORY_TYPE
                ),
                (5 * MB + 0x2000, 6 * MB, T::EFI_RUNTIME_SERVICES_CODE),
                (6 * MB, 7 * MB + 0x40000, T::EFI_BOOT_SERVICES_DATA),
                (7 * MB + 0x40000, 16 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
                (16 * MB, 32 * MB, T::EFI_CONVENTIONAL_MEMORY),
                (32 * MB, 64 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
                (4096 * MB, 4160 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
            ]
        );
        accepted.sort_by_key(|range| range.start());
        assert_eq!(
            accepted,
            [
                MemoryRange::new(0..0x10000),
                MemoryRange::new(6 * MB..7 * MB),
                MemoryRange::new(7 * MB..7 * MB + 0x40000),
                MemoryRange::new(16 * MB..32 * MB),
            ]
        );
    }

    #[test]
    fn rejects_host_ram_that_misses_measured_ram() {
        let platform = test_platform();
        let ram = [Ram {
            range: MemoryRange::new(MB..64 * MB),
            vnode: 0,
        }];
        let layout = ImageLayout {
            ram: &ram,
            measured_ram_end: 48 * MB,
            omitted: &[],
            platform: &platform,
            shim: MemoryRange::new(5 * MB..6 * MB),
        };
        assert_eq!(
            build_memory_map(&layout).err(),
            Some(Error::InvalidParameter)
        );
    }
}
