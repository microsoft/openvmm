// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI table handling for UEFI environment.
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::AtomicPtr;

use acpi_spec::Header;
use acpi_spec::Rsdp;
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
    fn new(rsdp_ptr: NonNull<Rsdp>) -> TmkResult<Self> {
        // SAFETY: The caller (from_uefi_system_table) obtains rsdp_ptr from the UEFI
        // configuration table, which guarantees it points to a valid, aligned Rsdp
        // structure that remains mapped for the lifetime of the system. The slice
        // covers exactly size_of::<Rsdp>() bytes starting at that address.
        let source = unsafe {
            core::slice::from_raw_parts(rsdp_ptr.as_ptr() as *const u8, size_of::<Rsdp>())
        };
        let rsdp = Rsdp::read_from_bytes(source).map_err(|e| {
            log::error!("Failed to parse RSDP: {:?}", e);
            AcpiWrapError::InvalidRsdpStructure
        })?;

        if &rsdp.signature != b"RSD PTR " {
            log::error!("Invalid RSDP signature: {:?}", rsdp.signature);
            return Err(AcpiWrapError::InvalidRsdpStructure.into());
        }

        if rsdp.revision < 2 {
            log::error!(
                "Unsupported RSDP revision: {}, expected >= 2",
                rsdp.revision
            );
            return Err(AcpiWrapError::InvalidRsdpStructure.into());
        }

        Ok(RsdpParser { rsdp })
    }

    // Creates an RsdpParser by locating the RSDP pointer from the UEFI system table.
    fn from_uefi_system_table() -> TmkResult<Self> {
        let rsdp_ptr = Self::find_rsdp_from_uefi_system_table()?;
        Self::new(rsdp_ptr)
    }

    // Retrieves the XSDT pointer from the RSDP structure.
    fn get_xsdt_ptr(&self) -> TmkResult<NonNull<Header>> {
        NonNull::new(self.rsdp.xsdt as *mut Header).ok_or(AcpiWrapError::InvalidXsdt.into())
    }

    // Finds the RSDP pointer from the UEFI system table.
    fn find_rsdp_from_uefi_system_table() -> TmkResult<NonNull<Rsdp>> {
        let system_table = uefi::table::system_table_raw();

        let Some(system_table) = system_table else {
            return Err(AcpiWrapError::UefiSystemTableNotFound.into());
        };

        // SAFETY: system_table_raw() returns a pointer that was set during UEFI entry
        // point initialization by the uefi crate. It points to a valid SystemTable
        // that remains valid until boot services are exited.
        let system_table_address = unsafe { system_table.as_ref() };

        let config_count = system_table_address.number_of_configuration_table_entries;
        let config_table_ptr = system_table_address.configuration_table;

        if config_count == 0 || config_table_ptr.is_null() {
            return Err(AcpiWrapError::RsdpNotFound.into());
        }

        // SAFETY: The UEFI specification guarantees that configuration_table points to
        // a contiguous array of exactly number_of_configuration_table_entries valid
        // ConfigurationTable entries within boot-services memory. We checked above
        // that config_count > 0 and config_table_ptr is non-null.
        let config_slice = unsafe { core::slice::from_raw_parts(config_table_ptr, config_count) };

        let rsdp = config_slice
            .iter()
            .find(|entry| entry.vendor_guid == ACPI2_GUID)
            .map(|entry| entry.vendor_table);

        if let Some(rsdp) = rsdp {
            Ok(NonNull::new(rsdp as *mut Rsdp).ok_or(AcpiWrapError::InvalidRsdp)?)
        } else {
            log::error!("ACPI2 RSDP not found");
            Err(AcpiWrapError::RsdpNotFound.into())
        }
    }
}

struct XSdtParser {
    entries: Vec<u64>,
}

impl XSdtParser {
    // Creates a new XSdtParser from a given XSDT header, validating the
    // signature and length before parsing entries.
    fn new(xsdt: &Header) -> TmkResult<Self> {
        if &xsdt.signature != b"XSDT" {
            return Err(AcpiWrapError::InvalidXsdtStructure.into());
        }

        let sdt_length = xsdt.length.get() as usize;
        let sdt_header_size = size_of::<Header>();

        if sdt_length < sdt_header_size {
            return Err(AcpiWrapError::InvalidXsdtStructure.into());
        }

        let sdt_address = xsdt as *const Header as usize;

        let entries_region_size = sdt_length - sdt_header_size;

        if entries_region_size % size_of::<u64>() != 0 {
            return Err(AcpiWrapError::InvalidXsdtStructure.into());
        }

        let entries_ptr = sdt_address + sdt_header_size;

        // SAFETY: We validated that sdt_length >= sdt_header_size and that
        // entries_region_size is an exact multiple of 8 bytes, so the slice
        // stays within the XSDT table boundary. The XSDT pointer was obtained
        // from the RSDP, which the firmware guarantees is a valid, mapped table.
        let entries_ptr_bytes =
            unsafe { core::slice::from_raw_parts(entries_ptr as *const u8, entries_region_size) };

        // create slice of u64 pointers
        let entries_slice = entries_ptr_bytes
            .chunks_exact(8)
            .filter_map(|chunk| chunk.try_into().ok().map(u64::from_le_bytes))
            .collect::<Vec<u64>>();

        Ok(XSdtParser {
            entries: entries_slice,
        })
    }

    // Iterate over all ACPI tables referenced by the XSDT.
    fn iter_tables(&self) -> impl Iterator<Item = NonNull<Header>> + '_ {
        self.entries
            .iter()
            .filter_map(|addr| NonNull::new(*addr as *mut Header))
    }

    // Find an ACPI table by its signature.
    fn find_table_by_signature(&self, signature: &[u8; 4]) -> Option<NonNull<Header>> {
        self.iter_tables().find(|sdt_ptr| {
            // SAFETY: Each XSDT entry is a physical address pointing to a valid ACPI
            // table header, set by the firmware. ACPI tables are required by
            // specification to be naturally aligned in memory. iter_tables already
            // filters out null pointers. The referenced memory is in the ACPI
            // reclaim region and remains mapped after exit_boot_services.
            let sdt_header = unsafe { sdt_ptr.as_ref() };
            &sdt_header.signature == signature
        })
    }
}

pub(crate) struct AcpiTableContext {
    _xsdt: AtomicPtr<Header>,
    madt: AtomicPtr<Header>,
}

impl AcpiTableContext {
    pub(crate) fn init() -> TmkResult<()> {
        ACPI_TABLE_CONTEXT.try_call_once(|| -> TmkResult<AcpiTableContext> {
            let rsdp_parser = RsdpParser::from_uefi_system_table()?;
            let xsdt = rsdp_parser.get_xsdt_ptr()?;
            // SAFETY: The XSDT pointer was obtained from the RSDP, which the firmware
            // guarantees points to a valid, properly aligned XSDT in the ACPI reclaim
            // region. This memory remains mapped after exit_boot_services.
            let xsdt_ref = unsafe { xsdt.as_ref() };
            let xsdt_parser = XSdtParser::new(xsdt_ref)?;
            let madt = xsdt_parser
                .find_table_by_signature(b"APIC")
                .ok_or(TmkError::NotFound)?;

            let context = AcpiTableContext {
                _xsdt: AtomicPtr::new(xsdt.as_ptr()),
                madt: AtomicPtr::new(madt.as_ptr()),
            };
            Ok(context)
        })?;
        Ok(())
    }

    /// Returns the number of APIC entries found in the MADT table.
    pub(crate) fn get_apic_count_from_madt() -> TmkResult<usize> {
        let acpi_ctx = ACPI_TABLE_CONTEXT
            .get()
            .ok_or(AcpiWrapError::InitializationError)?;
        let madt_ptr = NonNull::new(acpi_ctx.madt.load(core::sync::atomic::Ordering::Acquire));
        let madt_ptr = madt_ptr.ok_or(AcpiWrapError::InvalidMadt)?;
        // SAFETY: madt_ptr was stored during init() from the XSDT table walk. The ACPI
        // table memory is in the ACPI reclaim region and remains mapped. The Header
        // at this address is valid and properly aligned per the ACPI specification.
        let madt_table_size: usize = unsafe { madt_ptr.as_ref().length.get() } as usize;

        if madt_table_size < size_of::<Header>() {
            return Err(AcpiWrapError::InvalidMadtStructure.into());
        }

        // SAFETY: madt_ptr points to a valid MADT in the ACPI reclaim region (stored
        // during init). We validated that madt_table_size >= size_of::<Header>(), so
        // the slice does not extend beyond the table boundary.
        let madt_table_bytes =
            unsafe { core::slice::from_raw_parts(madt_ptr.as_ptr() as *const u8, madt_table_size) };
        let madt_parser = MadtParser::new(madt_table_bytes).map_err(|e| {
            log::error!("Failed to parse MADT table: {:?}", e);
            AcpiWrapError::InvalidMadtStructure
        })?;
        let apic_ids = madt_parser.parse_apic_ids().map_err(|e| {
            log::error!("Failed to parse MADT APIC IDs: {:?}", e);
            AcpiWrapError::InvalidMadtStructure
        })?;

        let processor_count = apic_ids.iter().filter(|id| id.is_some()).count();

        if processor_count == 0 {
            log::warn!("MADT contains no enabled APIC/X2APIC entries; processor count is 0");
        }

        Ok(processor_count)
    }
}

#[derive(Error, Debug)]
enum AcpiWrapError {
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
        log::error!("ACPI error: {:?}", err);
        final_err
    }
}
