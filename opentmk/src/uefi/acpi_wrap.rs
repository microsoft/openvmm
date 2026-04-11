// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI table handling for UEFI environment.
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::AtomicPtr;

use acpi_spec::Header;
use acpi_spec::Rsdp;
use acpi_spec::Table;
use acpi_spec::fadt::FADT_HW_REDUCED_ACPI;
use acpi_spec::fadt::Fadt;
use acpi_spec::fadt::GenericAddress;
use acpi_spec::madt::MadtParser;
use alloc::vec::Vec;
use spin::Once;
use uefi::table::cfg::ACPI2_GUID;
use zerocopy::FromBytes;

use crate::tmkdefs::AcpiError;
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
            AcpiError::InvalidRsdpStructure
        })?;

        if &rsdp.signature != b"RSD PTR " {
            log::error!("Invalid RSDP signature: {:?}", rsdp.signature);
            return Err(AcpiError::InvalidRsdpStructure.into());
        }

        if rsdp.revision < 2 {
            log::error!(
                "Unsupported RSDP revision: {}, expected >= 2",
                rsdp.revision
            );
            return Err(AcpiError::InvalidRsdpStructure.into());
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
        NonNull::new(self.rsdp.xsdt as *mut Header).ok_or_else(|| AcpiError::InvalidXsdt.into())
    }

    // Finds the RSDP pointer from the UEFI system table.
    fn find_rsdp_from_uefi_system_table() -> TmkResult<NonNull<Rsdp>> {
        let system_table = uefi::table::system_table_raw();

        let Some(system_table) = system_table else {
            return Err(AcpiError::UefiSystemTableNotFound.into());
        };

        // SAFETY: system_table_raw() returns a pointer that was set during UEFI entry
        // point initialization by the uefi crate. It points to a valid SystemTable
        // that remains valid until boot services are exited.
        let system_table_address = unsafe { system_table.as_ref() };

        let config_count = system_table_address.number_of_configuration_table_entries;
        let config_table_ptr = system_table_address.configuration_table;

        if config_count == 0 || config_table_ptr.is_null() {
            return Err(AcpiError::RsdpNotFound.into());
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
            NonNull::new(rsdp as *mut Rsdp).ok_or_else(|| AcpiError::RsdpNotFound.into())
        } else {
            Err(AcpiError::RsdpNotFound.into())
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
            return Err(AcpiError::InvalidXsdtStructure.into());
        }

        let sdt_length = xsdt.length.get() as usize;
        let sdt_header_size = size_of::<Header>();

        if sdt_length < sdt_header_size {
            return Err(AcpiError::InvalidXsdtStructure.into());
        }

        let sdt_address = xsdt as *const Header as usize;

        let entries_region_size = sdt_length - sdt_header_size;

        if entries_region_size % size_of::<u64>() != 0 {
            return Err(AcpiError::InvalidXsdtStructure.into());
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
    _fadt: AtomicPtr<Header>,
    sleep_mechanism: AcpiSleepMechanism,
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

            let fadt = xsdt_parser
                .find_table_by_signature(&Fadt::SIGNATURE)
                .ok_or(AcpiError::FadtNotFound)?;

            let sleep_mechanism = Self::parse_sleep_mechanism(fadt)?;

            let context = AcpiTableContext {
                _xsdt: AtomicPtr::new(xsdt.as_ptr()),
                madt: AtomicPtr::new(madt.as_ptr()),
                _fadt: AtomicPtr::new(fadt.as_ptr()),
                sleep_mechanism,
            };
            Ok(context)
        })?;
        Ok(())
    }

    /// Returns the number of APIC entries found in the MADT table.
    pub(crate) fn get_apic_count_from_madt() -> TmkResult<usize> {
        let acpi_ctx = ACPI_TABLE_CONTEXT
            .get()
            .ok_or(AcpiError::InitializationError)?;
        let madt_ptr = NonNull::new(acpi_ctx.madt.load(core::sync::atomic::Ordering::Acquire));
        let madt_ptr = madt_ptr.ok_or(AcpiError::InvalidMadt)?;
        // SAFETY: madt_ptr was stored during init() from the XSDT table walk. The ACPI
        // table memory is in the ACPI reclaim region and remains mapped. The Header
        // at this address is valid and properly aligned per the ACPI specification.
        let madt_table_size: usize = unsafe { madt_ptr.as_ref().length.get() } as usize;

        if madt_table_size < size_of::<Header>() {
            return Err(AcpiError::InvalidMadtStructure.into());
        }

        // SAFETY: madt_ptr points to a valid MADT in the ACPI reclaim region (stored
        // during init). We validated that madt_table_size >= size_of::<Header>(), so
        // the slice does not extend beyond the table boundary.
        let madt_table_bytes =
            unsafe { core::slice::from_raw_parts(madt_ptr.as_ptr() as *const u8, madt_table_size) };
        let madt_parser = MadtParser::new(madt_table_bytes).map_err(|e| {
            log::error!("Failed to parse MADT table: {:?}", e);
            AcpiError::InvalidMadtStructure
        })?;
        let apic_ids = madt_parser.parse_apic_ids().map_err(|e| {
            log::error!("Failed to parse MADT APIC IDs: {:?}", e);
            AcpiError::InvalidMadtStructure
        })?;

        let processor_count = apic_ids.iter().filter(|id| id.is_some()).count();

        if processor_count == 0 {
            log::warn!("MADT contains no enabled APIC/X2APIC entries; processor count is 0");
        }

        Ok(processor_count)
    }

    /// Returns the cached ACPI shutdown mechanism from the FADT.
    pub(crate) fn get_sleep_mechanism() -> TmkResult<AcpiSleepMechanism> {
        let acpi_ctx = ACPI_TABLE_CONTEXT
            .get()
            .ok_or(AcpiError::InitializationError)?;
        Ok(acpi_ctx.sleep_mechanism.clone())
    }

    /// Parse the FADT to determine the ACPI sleep mechanism.
    ///
    /// Supports both full-size ACPI 5.0+ FADTs with extended fields and
    /// shorter ACPI 1.0/2.0 FADTs that only have legacy PM1 control blocks.
    fn parse_sleep_mechanism(fadt_nn: NonNull<Header>) -> TmkResult<AcpiSleepMechanism> {
        // SAFETY: fadt_nn was obtained from the XSDT table walk.
        // The ACPI table memory is in the ACPI reclaim region and remains
        // mapped after exit_boot_services.
        let fadt_header = unsafe { fadt_nn.as_ref() };
        let fadt_table_size = fadt_header.length.get() as usize;

        if fadt_table_size <= size_of::<Header>() {
            log::error!("FADT table has no body (size={})", fadt_table_size);
            return Err(AcpiError::InvalidFadtStructure.into());
        }

        let body_size = fadt_table_size - size_of::<Header>();

        // SAFETY: We validated that the table has a body beyond the header.
        // The pointer arithmetic stays within the table boundary.
        let fadt_body_ptr = unsafe { (fadt_nn.as_ptr() as *const u8).add(size_of::<Header>()) };

        // Helper to read a field from the FADT body if the table is large
        // enough to contain it.  Returns the field value or a default.
        //
        // INVARIANT: This macro assumes `Fadt` is the body-only portion
        // of the FADT table (i.e. it does NOT include the ACPI Header).
        // `offset_of!(Fadt, field)` gives a body-relative offset, so
        // `fadt_body_ptr.add(offset)` lands at the correct byte. If
        // `Fadt` is ever changed to include the Header, these offsets
        // will silently break.
        macro_rules! fadt_field {
            ($field:ident, $ty:ty, $default:expr) => {{
                let offset = core::mem::offset_of!(Fadt, $field);
                let end = offset + size_of::<$ty>();
                if body_size >= end {
                    // SAFETY: We checked that the FADT body extends to at
                    // least `end` bytes, so this read is within bounds.
                    // Fadt is #[repr(C, packed)] so we use read_unaligned.
                    unsafe {
                        let ptr = fadt_body_ptr.add(offset);
                        core::ptr::read_unaligned(ptr as *const $ty)
                    }
                } else {
                    $default
                }
            }};
        }

        let flags: u32 = fadt_field!(flags, u32, 0u32);
        let pm1a: u32 = fadt_field!(pm1a_cnt_blk, u32, 0u32);
        let pm1b: u32 = fadt_field!(pm1b_cnt_blk, u32, 0u32);

        let hw_reduced = (flags & FADT_HW_REDUCED_ACPI) != 0;

        // Try extended x_pm1a/b_cnt_blk fields (ACPI 2.0+, offset 148+).
        let x_pm1a: GenericAddress =
            fadt_field!(x_pm1a_cnt_blk, GenericAddress, GenericAddress::default());
        let x_pm1b: GenericAddress =
            fadt_field!(x_pm1b_cnt_blk, GenericAddress, GenericAddress::default());
        let sleep_reg: GenericAddress =
            fadt_field!(sleep_control_reg, GenericAddress, GenericAddress::default());

        // Prefer extended addresses over legacy.
        let x_pm1a_addr = x_pm1a.address;
        let effective_pm1a = if x_pm1a_addr != 0 {
            x_pm1a_addr as u16
        } else {
            pm1a as u16
        };

        let x_pm1b_addr = x_pm1b.address;
        let effective_pm1b = if x_pm1b_addr != 0 {
            x_pm1b_addr as u16
        } else {
            pm1b as u16
        };

        let sleep_addr = sleep_reg.address;

        if !hw_reduced && effective_pm1a != 0 {
            log::info!(
                "Legacy ACPI: x_pm1a_cnt=0x{:x}, x_pm1b_cnt=0x{:x}",
                effective_pm1a,
                effective_pm1b
            );
            Ok(AcpiSleepMechanism::Legacy {
                pm1a_cnt_blk: effective_pm1a,
                pm1b_cnt_blk: effective_pm1b,
            })
        } else if sleep_addr != 0 {
            let space = sleep_reg.addr_space_id;
            log::info!(
                "HW-reduced ACPI: sleep_control_reg addr_space={:?}, addr=0x{:x}",
                space,
                sleep_addr
            );
            Ok(AcpiSleepMechanism::HwReduced { sleep_reg })
        } else {
            log::error!(
                "No usable ACPI shutdown mechanism: hw_reduced={}, pm1a=0x{:x}, sleep_addr=0x{:x}",
                hw_reduced,
                pm1a,
                sleep_addr
            );
            Err(AcpiError::InvalidFadtStructure.into())
        }
    }
}

/// Describes how to trigger an ACPI S5 shutdown on this platform.
#[derive(Clone)]
pub(crate) enum AcpiSleepMechanism {
    /// Legacy ACPI: write to PM1a/PM1b control block I/O ports.
    Legacy {
        /// PM1a control block I/O port address.
        pm1a_cnt_blk: u16,
        /// PM1b control block I/O port address (zero if absent).
        pm1b_cnt_blk: u16,
    },
    /// HW-reduced ACPI: write to the sleep control register.
    HwReduced {
        /// Sleep control register Generic Address.
        sleep_reg: GenericAddress,
    },
}
