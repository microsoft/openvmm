// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common helper routines for all loaders.

use crate::importer::BootPageAcceptance;
use crate::importer::ImageLoad;
use crate::importer::SegmentRegister;
use crate::importer::TableRegister;
use crate::importer::X86Register;
use hvdef::HV_PAGE_SIZE;
use vm_topology::memory::MemoryLayout;
use x86defs::GdtEntry;
use x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES;
use x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const DEFAULT_GDT_COUNT: usize = 4;
/// The size of the default GDT table, in bytes.
pub const DEFAULT_GDT_SIZE: u64 = HV_PAGE_SIZE;

/// Import a default GDT at the given address, with one page imported.
/// The GDT is used with cs as entry 1, and data segments (ds, es, fs, gs, ss) as entry 2.
/// Registers using the GDT are imported with vtl 0 only.
pub fn import_default_gdt(
    importer: &mut dyn ImageLoad<X86Register>,
    gdt_page_base: u64,
) -> anyhow::Result<()> {
    // Create a default GDT consisting of two entries.
    // ds, es, fs, gs, ss are entry 2 (linear_selector)
    // cs is entry 1 (linear_code64_selector)
    let default_data_attributes: u16 = X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.into();
    let default_code_attributes: u16 = X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.into();
    let gdt: [GdtEntry; DEFAULT_GDT_COUNT] = [
        GdtEntry::new_zeroed(),
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_code_attributes as u8,
            attr_high: (default_code_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_data_attributes as u8,
            attr_high: (default_data_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
        GdtEntry::new_zeroed(),
    ];
    let gdt_entry_size = size_of::<GdtEntry>();
    let linear_selector_offset = 2 * gdt_entry_size;
    let linear_code64_selector_offset = gdt_entry_size;

    // Import the GDT into the specified base page.
    importer.import_pages(
        gdt_page_base,
        DEFAULT_GDT_SIZE / HV_PAGE_SIZE,
        "default-gdt",
        BootPageAcceptance::Exclusive,
        gdt.as_bytes(),
    )?;

    // Import GDTR and selectors.
    let mut import_reg = |register| importer.import_vp_register(register);
    import_reg(X86Register::Gdtr(TableRegister {
        base: gdt_page_base * HV_PAGE_SIZE,
        limit: (size_of::<GdtEntry>() * DEFAULT_GDT_COUNT - 1) as u16,
    }))?;

    let ds = SegmentRegister {
        selector: linear_selector_offset as u16,
        base: 0,
        limit: 0xffffffff,
        attributes: default_data_attributes,
    };
    import_reg(X86Register::Ds(ds))?;
    import_reg(X86Register::Es(ds))?;
    import_reg(X86Register::Fs(ds))?;
    import_reg(X86Register::Gs(ds))?;
    import_reg(X86Register::Ss(ds))?;

    let cs = SegmentRegister {
        selector: linear_code64_selector_offset as u16,
        base: 0,
        limit: 0xffffffff,
        attributes: default_code_attributes,
    };
    import_reg(X86Register::Cs(cs))?;

    Ok(())
}

/// Computes the x86 variable MTRRs that describe the given memory layout. This
/// is intended to be used to setup MTRRs for booting a guest with two mmio
/// gaps, such as booting Linux, UEFI, or PCAT.
///
/// N.B. Currently this panics if there are not exactly two MMIO ranges.
pub fn compute_variable_mtrrs(memory: &MemoryLayout) -> Vec<X86Register> {
    const WRITEBACK: u64 = 0x6;

    assert_eq!(
        memory.mmio().len(),
        2,
        "only two MMIO gaps are supported currently"
    );

    let mmio_gap_low = memory.mmio()[0];
    let mmio_gap_high = memory.mmio()[1];

    // Clamp the GpaSpaceSize to something reasonable
    let gpa_space_size = memory.physical_address_size().clamp(36, 52);

    // The MMIO limits will be the basis of the MTRR calculations
    // as page count doesn't work when there may be gaps between memory blocks.

    let mut result = Vec::with_capacity(8);

    // Our PCAT firmware sets MTRR 200 and MTRR Mask 201 to 128 MB during boot, so we
    // mimic that here.
    let pcat_mtrr_size = 128 * 1024 * 1024;

    result.push(X86Register::MtrrPhysBase0(WRITEBACK));
    result.push(X86Register::MtrrPhysMask0(mtrr_mask(
        gpa_space_size,
        pcat_mtrr_size - 1,
    )));

    // If there is more than 128 MB, use MTRR 202 and MTRR Mask 203 to cover the
    // amount of memory below the 3.8GB memory gap.
    if memory.end_of_ram() > pcat_mtrr_size {
        result.push(X86Register::MtrrPhysBase1(pcat_mtrr_size | WRITEBACK));
        result.push(X86Register::MtrrPhysMask1(mtrr_mask(
            gpa_space_size,
            mmio_gap_low.start() - 1,
        )));
    }

    // If there is more than ~3.8GB of memory, use MTRR 204 and MTRR Mask 205 to cover
    // the amount of memory above 4GB.
    if memory.end_of_ram() > mmio_gap_low.end() {
        result.push(X86Register::MtrrPhysBase2(mmio_gap_low.end() | WRITEBACK));
        result.push(X86Register::MtrrPhysMask2(mtrr_mask(
            gpa_space_size,
            mmio_gap_high.start() - 1,
        )));
    }

    // If there is more memory than 64GB then use MTRR 206 and MTRR Mask 207 and possibly
    // MTRR 208 and MTRR Mask 209 depending on maximum address width. Both MTRR pairs are
    // used with the magic 8TB boundary to work around a bug in older Linux kernels
    // (e.g. RHEL 6.x, etc.)
    if memory.end_of_ram() > mmio_gap_high.end() {
        result.push(X86Register::MtrrPhysBase3(mmio_gap_high.end() | WRITEBACK));
        result.push(X86Register::MtrrPhysMask3(mtrr_mask(
            gpa_space_size,
            (1 << std::cmp::min(gpa_space_size, 43)) - 1,
        )));
        if gpa_space_size > 43 {
            result.push(X86Register::MtrrPhysBase4((1 << 43) | WRITEBACK));
            result.push(X86Register::MtrrPhysMask4(mtrr_mask(
                gpa_space_size,
                (1 << gpa_space_size) - 1,
            )));
        }
    }

    result
}

fn mtrr_mask(gpa_space_size: u8, maximum_address: u64) -> u64 {
    const ENABLED: u64 = 1 << 11;

    let mut result = ENABLED;

    // Set all the bits above bit 11 to 1's to cover the gpa_space_size
    for index in 12..gpa_space_size {
        result |= 1 << index;
    }

    // Clear the span of bits above bit 11 to cover the maximum address
    for index in 12..gpa_space_size {
        let test_maximum_address = 1 << index;

        if maximum_address >= test_maximum_address {
            // Turn the correct bit off
            result &= !(1 << index);
        } else {
            // Done clearing the span of bits
            break;
        }
    }

    result
}
