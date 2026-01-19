// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI table handling for UEFI environment.
use core::ffi::c_void;
use core::mem::size_of;
use core::mem::transmute;
use core::ptr::NonNull;
use core::sync::atomic::AtomicPtr;

use acpi_spec::Header;
use acpi_spec::Rsdp;
use acpi_spec::madt::MadtEntry;
use acpi_spec::madt::MadtParser;
use alloc::vec::Vec;
use spin::Once;
use thiserror::Error;
use uefi::table::cfg::ACPI2_GUID;
use zerocopy::FromBytes;

use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

static ACPI_TABLE_CONTEXT: Once<AcpiTableContext> = Once::new();

struct RsdpParser {
    rsdp: Rsdp,
}

impl RsdpParser {
    // Creates a new RsdpParser from a given RSDP pointer.
    pub fn new(rsdp_ptr: NonNull<Rsdp>) -> TmkResult<Self> {
        // SAFETY: rsdp_ptr is valid as ensured by the constructor
        let source = unsafe {
            core::slice::from_raw_parts(rsdp_ptr.as_ptr() as *const u8, size_of::<Rsdp>())
        };
        let rsdp =
            Rsdp::read_from_bytes(source).map_err(|_| AcpiWrapError::InvalidRsdpStructure)?;
        Ok(RsdpParser { rsdp })
    }

    // Creates an RsdpParser by locating the RSDP pointer from the UEFI system table.
    pub fn from_uefi_system_table() -> TmkResult<Self> {
        let rsdp_ptr = Self::find_rsdp_from_uefi_system_table()?;
        Ok(Self::new(rsdp_ptr)?)
    }

    // Retrieves the XSDT pointer from the RSDP structure.
    pub fn get_xsdt_ptr(&self) -> TmkResult<NonNull<Header>> {
        let xsdt_address: usize = self.rsdp.xsdt as usize;
        Ok(NonNull::new(xsdt_address as *mut Header).ok_or(AcpiWrapError::InvalidXsdt)?)
    }

    // Finds the RSDP pointer from the UEFI system table.
    fn find_rsdp_from_uefi_system_table() -> AcpiWrapResult<NonNull<Rsdp>> {
        let system_table = uefi::table::system_table_raw();

        let Some(system_table) = system_table else {
            return Err(AcpiWrapError::UefiSystemTableNotFound);
        };

        // SAFETY: system_table is valid as ensured by uefi::table::system_table_raw
        let system_table_address = unsafe { system_table.as_ref() };

        let config_count = system_table_address.number_of_configuration_table_entries;
        let config_table_ptr = system_table_address.configuration_table;

        // SAFETY: UEFI guarantees that the configuration table pointer is valid for the number of entries
        let config_slice = unsafe { core::slice::from_raw_parts(config_table_ptr, config_count) };

        let rsdp = config_slice
            .iter()
            .find(|entry| entry.vendor_guid == ACPI2_GUID)
            .map(|entry| entry.vendor_table);

        if let Some(rsdp) = rsdp {
            Ok(NonNull::new(rsdp as *mut Rsdp).ok_or(AcpiWrapError::InvalidRsdp)?)
        } else {
            log::error!("ACPI2 RSDP not found");
            Err(AcpiWrapError::RsdpNotFound)
        }
    }
}

struct XSdtParser {
    entries: Vec<u64>,
}

impl XSdtParser {
    // Creates a new XSdtParser from a given XSDT pointer.
    pub fn new(xsdt: &Header) -> Self {
        let sdt_address = xsdt as *const Header as usize;

        let sdt_length = xsdt.length.get() as usize;
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

        let parser = XSdtParser {
            entries: entries_slice,
        };
        parser
    }

    // Iterate over all ACPI tables referenced by the XSDT.
    pub fn iter_tables(&self) -> impl Iterator<Item = NonNull<Header>> + '_ {
        self.entries.iter().filter_map(|addr| {
            let sdt_header = unsafe { transmute::<*const c_void, &Header>(*addr as *const c_void) };
            let sdt_table_ptr = sdt_header as *const Header;
            NonNull::new(sdt_table_ptr as *mut Header)
        })
    }

    // Find an ACPI table by its signature.
    pub fn find_table_by_signature(&self, signature: &[u8; 4]) -> Option<NonNull<Header>> {
        self.iter_tables().find(|sdt_ptr| {
            // SAFETY: sdt_ptr is valid as ensured by iter_tables
            let sdt_header = unsafe { sdt_ptr.as_ref() };
            &sdt_header.signature == signature
        })
    }
}

pub struct AcpiTableContext {
    _xsdt: AtomicPtr<Header>,
    _madt: AtomicPtr<Header>,
}

impl AcpiTableContext {
    pub fn init() -> TmkResult<()> {
        ACPI_TABLE_CONTEXT.try_call_once(|| -> TmkResult<AcpiTableContext> {
            let rsdp_parser = RsdpParser::from_uefi_system_table()?;
            let xsdt = rsdp_parser.get_xsdt_ptr()?;
            // SAFETY: xsdt is valid as ensured by UEFI specification
            let xsdt_ref = unsafe { xsdt.as_ref() };
            let xsdt_parser = XSdtParser::new(xsdt_ref);
            let madt = xsdt_parser
                .find_table_by_signature(b"APIC")
                .ok_or(TmkError::NotFound)?;

            let context = AcpiTableContext {
                _xsdt: AtomicPtr::new(xsdt.as_ptr()),
                _madt: AtomicPtr::new(madt.as_ptr()),
            };
            Ok(context)
        })?;
        Ok(())
    }

    /// Returns the number of APIC entries found in the MADT table.
    pub fn get_apic_count_from_madt() -> TmkResult<usize> {
        let acpi_ctx = ACPI_TABLE_CONTEXT
            .get()
            .ok_or(AcpiWrapError::InitializationError)?;
        let madt_ptr = NonNull::new(acpi_ctx._madt.load(core::sync::atomic::Ordering::Acquire));
        let madt_ptr = madt_ptr.ok_or(AcpiWrapError::InvalidMadt)?;
        // SAFETY: madt_ptr is valid as ensured witin the constructor and ACPI specification
        let madt_table_size: usize = unsafe { madt_ptr.as_ref().length.get() } as usize;
        // SAFETY: madt_ptr is valid as ensured witin the constructor and ACPI specification
        let madt_table_bytes =
            unsafe { core::slice::from_raw_parts(madt_ptr.as_ptr() as *const u8, madt_table_size) };
        let madt_parser = MadtParser::new(madt_table_bytes).map_err(|e| {
            log::error!("Failed to parse MADT table: {:?}", e);
            AcpiWrapError::InvalidMadtStructure
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
}

type AcpiWrapResult<T> = Result<T, AcpiWrapError>;
#[derive(Error, Debug)]
pub enum AcpiWrapError {
    #[error("ACPI table initialization error")]
    InitializationError,
    #[error("UEFI system table not found")]
    UefiSystemTableNotFound,
    #[error("Invalid RSDP address")]
    InvalidRsdp,
    #[error("Invalid RSDP structure")]
    InvalidRsdpStructure,
    #[error("Invalid XSDT address")]
    InvalidXsdt,
    #[error("RSDP not found")]
    RsdpNotFound,
    #[error("Invalid MADT address")]
    InvalidMadt,
    #[error("Invalid MADT structure")]
    InvalidMadtStructure,
    #[error("Invalid XSDT structure")]
    InvalidXsdtStructure,
}

impl From<AcpiWrapError> for TmkError {
    fn from(err: AcpiWrapError) -> TmkError {
        let final_err = match err {
            AcpiWrapError::InitializationError
            | AcpiWrapError::UefiSystemTableNotFound
            | AcpiWrapError::InvalidRsdp
            | AcpiWrapError::InvalidRsdpStructure
            | AcpiWrapError::InvalidXsdt
            | AcpiWrapError::RsdpNotFound
            | AcpiWrapError::InvalidMadt
            | AcpiWrapError::InvalidMadtStructure
            | AcpiWrapError::InvalidXsdtStructure => TmkError::AcpiError,
        };
        log::info!("Converting {:?} to {:?}", err, final_err);
        final_err
    }
}
