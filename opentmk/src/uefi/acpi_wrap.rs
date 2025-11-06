// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI table handling for UEFI environment.
use core::ffi::c_void;
use core::mem::size_of;
use core::mem::transmute;
use core::ptr::NonNull;

use acpi_spec::Header;
use acpi_spec::Rsdp;
use acpi_spec::madt::MadtEntry;
use acpi_spec::madt::MadtParser;
use alloc::vec::Vec;
use uefi::table::cfg::ACPI2_GUID;

use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

fn get_rsdp_ptr() -> TmkResult<NonNull<Rsdp>> {
    let system_table = uefi::table::system_table_raw();

    if system_table.is_none() {
        return Err(TmkError::AcpiError);
    }

    let mut system_table = system_table.unwrap();

    // SAFETY: system_table is valid as ensured by uefi::table::system_table_raw
    let system_table_address = unsafe { system_table.as_mut() };

    let config_count = system_table_address.number_of_configuration_table_entries;
    let config_table_ptr = system_table_address.configuration_table;

    // SAFETY: UEFI guarantees that the configuration table pointer is valid for the number of entries
    let config_slice = unsafe { core::slice::from_raw_parts(config_table_ptr, config_count) };

    let find = |guid| {
        config_slice
            .iter()
            .find(|entry| entry.vendor_guid == guid)
            .map(|entry| entry.vendor_table)
    };

    if let Some(rsdp) = find(ACPI2_GUID) {
        Ok(NonNull::new(rsdp as *mut Rsdp).ok_or(TmkError::AcpiError)?)
    } else {
        log::error!("ACPI2 RSDP not found");
        Err(TmkError::AcpiError)
    }
}

fn get_xsdt_ptr() -> TmkResult<NonNull<Header>> {
    let rsdp_ptr = get_rsdp_ptr()?;

    // SAFETY: rsdp_ptr is valid as ensured by get_rsdp_ptr
    let rsdp = unsafe { rsdp_ptr.as_ref() };

    let xsdt_address = rsdp.xsdt as usize;

    Ok(NonNull::new(xsdt_address as *mut Header).ok_or(TmkError::AcpiError)?)
}

fn get_madt_ptr() -> TmkResult<NonNull<Header>> {
    // From XSDT get SDT Header
    let sdt_ptr = get_xsdt_ptr()?;
    let sdt_address = sdt_ptr.as_ptr() as usize;

    // SAFETY: sdt_ptr is valid as ensured by get_xsdt_ptr
    let sdt_length = unsafe { sdt_ptr.as_ref().length.get() };
    let sdt_header_size = size_of::<Header>();

    // get number of entries pointing to other SDTs
    let entries_count = (sdt_length as usize - sdt_header_size) / size_of::<u64>();

    // pointer to pointer table of other SDTs
    let entries_ptr = sdt_address + sdt_header_size;

    // SAFETY: madt size is valid as ensured by UEFI specification
    let entries_ptr_bytes = unsafe {
        core::slice::from_raw_parts(entries_ptr as *const u8, entries_count * size_of::<u64>())
    };

    // create slice of u64 pointers
    let entries_slice = entries_ptr_bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<u64>>();

    // finding MADT table
    let madt = entries_slice
        .iter()
        .find(|addr| {
            let sdt_header =
                unsafe { transmute::<*const c_void, &Header>(**addr as *const c_void) };
            log::info!("Found SDT Table: {:x?}", sdt_header.signature);
            if sdt_header.signature == *b"APIC" {
                log::info!("Found MADT Table at address: {:x?}", addr);
            }
            sdt_header.signature == *b"APIC"
        })
        .map(|u64| {
            // SAFETY: the address is valid as it was found in the ACPI tables, UEFI guarantees their validity
            unsafe { transmute::<*const c_void, &Header>(*u64 as *const c_void) }
        });
    if let Some(madt) = madt {
        let madt_table_ptr = madt as *const Header;
        return Ok(NonNull::new(madt_table_ptr as *mut Header).ok_or(TmkError::AcpiError)?);
    }

    log::error!("MADT Table not found");
    Err(TmkError::AcpiError)
}

/// Returns the number of APIC entries found in the MADT table.
pub fn get_apic_count_from_madt() -> TmkResult<usize> {
    let madt_ptr = get_madt_ptr()?;
    // SAFETY: madt_ptr is valid as ensured by get_madt_ptr
    let madt_table_size: usize = unsafe { madt_ptr.as_ref().length.get() } as usize;
    // SAFETY: madt_ptr is valid as ensured by get_madt_ptr
    let madt_table_bytes =
        unsafe { core::slice::from_raw_parts(madt_ptr.as_ptr() as *const u8, madt_table_size) };
    let madt_parser = MadtParser::new(madt_table_bytes).map_err(|e| {
        log::error!("Failed to parse MADT table: {:?}", e);
        TmkError::AcpiError
    })?;
    let mut processor_count = 0;
    madt_parser.entries().for_each(|e| {
        if let Ok(entry) = e {
            log::trace!("MADT Entry: {:?}", entry);
            match entry {
                MadtEntry::Apic(_) | MadtEntry::X2Apic(_) => {
                    processor_count += 1;
                }
            }
        } else {
            log::error!("Failed to parse MADT entry: {:?}", e);
        }
    });

    Ok(processor_count)
}
