// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64 page table walking.

#![warn(missing_docs)]

use crate::emulate::EmuTranslateError;
use crate::emulate::EmuTranslateResult;
use crate::emulate::TranslateGvaSupport;
use crate::emulate::TranslateMode;
use aarch64defs::Cpsr64;
use aarch64defs::EsrEl2;
use aarch64defs::FaultStatusCode;
use aarch64defs::IntermPhysAddrSize;
use aarch64defs::IssDataAbort;
use aarch64defs::Pte;
use aarch64defs::SctlrEl1;
use aarch64defs::TranslationControlEl1;
use aarch64defs::TranslationGranule0;
use aarch64defs::TranslationGranule1;
use guestmem::GuestMemory;
use hvdef::hypercall::TranslateGvaControlFlagsArm64;
use hvdef::hypercall::TranslateGvaResultCode;
use hvdef::HV_PAGE_SHIFT;
use thiserror::Error;

/// Registers needed to walk the page table.
#[derive(Debug, Clone)]
pub struct TranslationRegisters {
    /// SPSR_EL2
    pub cpsr: Cpsr64,
    /// SCTLR_ELx
    pub sctlr: SctlrEl1,
    /// TCR_ELx
    pub tcr: TranslationControlEl1,
    /// TTBR0_ELx
    pub ttbr0: u64,
    /// TTBR1_ELx
    pub ttbr1: u64,
    /// EsrEl2
    pub syndrome: u64,

    /// The way the processor uses to determine if an access is to encrypted
    /// memory. This is used to enforce that page tables and executable code are
    /// in encrypted memory.
    pub encryption_mode: EncryptionMode,
}

/// The way the processor uses to determine if an access is to encrypted memory.
#[derive(Debug, Copy, Clone)]
pub enum EncryptionMode {
    /// Memory accesses below the virtual top of memory address are encrypted.
    Vtom(u64),
    /// No memory is encrypted.
    None,
}

/// Flags to control the page table walk.
#[derive(Debug, Clone)]
pub struct TranslateFlags {
    /// Validate a VP in the current state can execute from this GVA.
    pub validate_execute: bool,
    /// Validate a VP in the current state can read from this GVA.
    pub validate_read: bool,
    /// Validate a VP in the current state can write to this GVA.
    pub validate_write: bool,
    /// The type of privilege check to perform.
    pub privilege_check: TranslatePrivilegeCheck,
    /// Update the page table entries' access and dirty bits as appropriate.
    pub set_page_table_bits: bool,
}

/// The type of privilege check to perform.
#[derive(Debug, Copy, Clone)]
pub enum TranslatePrivilegeCheck {
    /// No privilege checks.
    None,
    /// Validate user-mode access.
    User,
    /// Validate supervisor access.
    Supervisor,
    /// Validate both supervisor and user-mode access.
    Both,
    /// Validate according to the current privilege level.
    CurrentPrivilegeLevel,
}

impl TranslateFlags {
    /// Return flags based on the `HvTranslateVirtualAddress` hypercall input
    /// flags.
    ///
    /// Note that not all flags are considered.
    pub fn from_hv_flags(flags: TranslateGvaControlFlagsArm64) -> Self {
        Self {
            validate_execute: flags.validate_execute(),
            validate_read: flags.validate_read(),
            validate_write: flags.validate_write(),
            privilege_check: if flags.pan_clear() {
                TranslatePrivilegeCheck::None
            } else if flags.user_access() {
                if flags.supervisor_access() {
                    TranslatePrivilegeCheck::Both
                } else {
                    TranslatePrivilegeCheck::User
                }
            } else if flags.supervisor_access() {
                TranslatePrivilegeCheck::Supervisor
            } else {
                TranslatePrivilegeCheck::CurrentPrivilegeLevel
            },
            set_page_table_bits: flags.set_page_table_bits(),
        }
    }
}

/// Translation error.
#[derive(Debug, Error)]
pub enum Error {
    /// Invalid address size
    #[error("invalid address size at level")]
    InvalidAddressSize(u8),
    /// The page table flags were invalid.
    #[error("invalid page table flags at level")]
    InvalidPageTableFlags(u8),
    /// A page table GPA was not mapped.
    #[error("gpa unmapped at level")]
    GpaUnmapped(u8),
    /// The page was not present in the page table.
    #[error("page not present at level")]
    PageNotPresent(u8),
    /// Accessing the GVA would create a privilege violation.
    #[error("privilege violation at level")]
    PrivilegeViolation(u8),
}

impl From<&Error> for TranslateGvaResultCode {
    fn from(err: &Error) -> TranslateGvaResultCode {
        match err {
            Error::InvalidAddressSize(_) => TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
            Error::InvalidPageTableFlags(_) => TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
            Error::GpaUnmapped(_) => TranslateGvaResultCode::GPA_UNMAPPED,
            Error::PageNotPresent(_) => TranslateGvaResultCode::PAGE_NOT_PRESENT,
            Error::PrivilegeViolation(_) => TranslateGvaResultCode::PRIVILEGE_VIOLATION,
        }
    }
}

impl From<Error> for TranslateGvaResultCode {
    fn from(err: Error) -> TranslateGvaResultCode {
        (&err).into()
    }
}

impl From<&Error> for EsrEl2 {
    fn from(err: &Error) -> EsrEl2 {
        let dfsc = match err {
            Error::InvalidAddressSize(i) => FaultStatusCode::ADDRESS_SIZE_FAULT_LEVEL0.0 + i,
            Error::InvalidPageTableFlags(i) => FaultStatusCode::TRANSLATION_FAULT_LEVEL0.0 + i,
            Error::GpaUnmapped(i) => FaultStatusCode::ACCESS_FLAG_FAULT_LEVEL0.0 + i,
            Error::PageNotPresent(i) => FaultStatusCode::ACCESS_FLAG_FAULT_LEVEL0.0 + i,
            Error::PrivilegeViolation(i) => FaultStatusCode::PERMISSION_FAULT_LEVEL0.0 + i,
        };
        let data_abort = IssDataAbort::new().with_dfsc(FaultStatusCode(dfsc));
        data_abort.into()
    }
}

impl From<Error> for EsrEl2 {
    fn from(err: Error) -> EsrEl2 {
        (&err).into()
    }
}

/// Emulates a page table walk.
///
/// This is suitable for implementing [`crate::emulate::EmulatorSupport::translate_gva`].
pub fn emulate_translate_gva<T: TranslateGvaSupport>(
    support: &mut T,
    gva: u64,
    mode: TranslateMode,
) -> Result<Result<EmuTranslateResult, EmuTranslateError>, T::Error> {
    // Always acquire the TLB lock for this path.
    support.acquire_tlb_lock();

    let registers = support.registers()?;
    let flags = TranslateFlags {
        validate_execute: matches!(mode, TranslateMode::Execute),
        validate_read: matches!(mode, TranslateMode::Execute | TranslateMode::Read),
        validate_write: matches!(mode, TranslateMode::Write),
        privilege_check: TranslatePrivilegeCheck::CurrentPrivilegeLevel,
        set_page_table_bits: true,
    };

    let r = match translate_gva_to_gpa(support.guest_memory(), gva, &registers, flags) {
        Ok(gpa) => Ok(EmuTranslateResult {
            gpa,
            overlay_page: None,
        }),
        Err(err) => {
            let mut syndrome: EsrEl2 = (&err).into();
            let cur_syndrome: EsrEl2 = registers.syndrome.into();
            syndrome.set_il(cur_syndrome.il());
            Err(EmuTranslateError {
                code: err.into(),
                event_info: Some(syndrome),
            })
        }
    };
    Ok(r)
}

struct Aarch64PageTable {
    pub table_address_gpa: u64,
    pub page_shift: u64,
    pub span_shift: u64,
    pub level: u64,
    pub level_width: u64,
    pub is_hierarchical_permissions: bool,
}

fn get_root_page_table(
    gva: u64,
    registers: &TranslationRegisters,
    flags: &TranslateFlags,
) -> Result<(u64, Aarch64PageTable), Error> {
    let use_ttbr1 = (gva & 0x00400000_00000000) != 0;
    let (
        root_address,
        address_width,
        granule_width,
        ignore_top_byte,
        ignore_top_byte_instruction,
        is_hierarchical_permissions,
    ) = if use_ttbr1 {
        let granule_width = match registers.tcr.tg1() {
            TranslationGranule1::TG_INVALID => return Err(Error::InvalidPageTableFlags(0)),
            TranslationGranule1::TG_16KB => 14,
            TranslationGranule1::TG_4KB => 12,
            TranslationGranule1::TG_64KB => 16,
            _ => return Err(Error::InvalidPageTableFlags(0)),
        };
        (
            registers.ttbr1 & ((1 << 48) - 2),
            registers.tcr.ttbr1_valid_address_bits(),
            granule_width,
            registers.tcr.tbi1() != 0,
            registers.tcr.tbid1() != 0,
            !registers.tcr.hpd1() != 0, // || processor does not support HPDS
        )
    } else {
        let granule_width = match registers.tcr.tg0() {
            TranslationGranule0::TG_4KB => 12,
            TranslationGranule0::TG_64KB => 16,
            TranslationGranule0::TG_16KB => 14,
            _ => return Err(Error::InvalidPageTableFlags(0)),
        };
        (
            registers.ttbr0 & ((1 << 48) - 2),
            registers.tcr.ttbr0_valid_address_bits(),
            granule_width,
            registers.tcr.tbi0() != 0,
            registers.tcr.tbid0() != 0,
            !registers.tcr.hpd0() != 0, // || processor does not support HPDS
        )
    };
    if !(25..=48).contains(&address_width) {
        tracing::trace!(address_width, "Invalid TCR value");
        return Err(Error::InvalidAddressSize(0));
    }
    let num_levels = (address_width - 1) / granule_width;
    if num_levels == 0 || num_levels > 4 {
        tracing::trace!(address_width, granule_width, "Invalid page hierarchy");
        return Err(Error::InvalidPageTableFlags(0));
    }
    let ignore_top_byte =
        ignore_top_byte && (!flags.validate_execute || ignore_top_byte_instruction);
    let high_mask = !((1 << address_width) - 1);
    let verify_high_bits = if use_ttbr1 {
        // TTBR1 addresses should have all the high bits set.
        let masked_address = gva
            | if ignore_top_byte {
                0xff000000_00000000
            } else {
                0
            };
        (masked_address & high_mask) == high_mask
    } else {
        // TTBR0 addresses should have all the high bits clear.
        let masked_address = gva
            & if ignore_top_byte {
                0x00ffffff_ffffffff
            } else {
                0xffffffff_ffffffff
            };
        (masked_address & high_mask) == 0
    };
    if !verify_high_bits {
        tracing::trace!(gva, address_width, "Invalid high bits");
        return Err(Error::InvalidAddressSize(0));
    }
    let span_shift = granule_width + (granule_width - 3) * (num_levels - 1);
    let level_width = address_width - span_shift;
    Ok((
        gva & !high_mask,
        Aarch64PageTable {
            table_address_gpa: root_address & !((1 << (level_width + 3)) - 1),
            page_shift: granule_width,
            span_shift,
            level: num_levels - 1,
            level_width,
            is_hierarchical_permissions,
        },
    ))
}

struct PageTableWalkContext<'a> {
    guest_memory: &'a GuestMemory,
    flags: TranslateFlags,
    check_user_access: bool,
    check_supervisor_access: bool,
    write_no_execute: bool,
    output_size_mask: u64,
}

enum PageTableWalkResult {
    Table(Aarch64PageTable),
    BaseGpa(u64, u64),
}

fn get_next_page_table(
    level: u8,
    address: u64,
    page_table: &Aarch64PageTable,
    context: &PageTableWalkContext<'_>,
    is_user_address: &mut bool,
    is_writeable_address: &mut bool,
    is_executable_address: &mut bool,
) -> Result<PageTableWalkResult, Error> {
    if page_table.table_address_gpa & context.output_size_mask != page_table.table_address_gpa {
        tracing::trace!(
            address,
            level = page_table.level,
            page_table_address = page_table.table_address_gpa,
            "Invalid page table address"
        );
        return Err(Error::InvalidAddressSize(level));
    }
    let index_mask = (1 << page_table.level_width) - 1;
    let pte_index = (address >> page_table.span_shift) & index_mask;
    let pte_gpa = page_table.table_address_gpa + (pte_index << 3);
    let mut pte_access = context
        .guest_memory
        .read_plain::<u64>(pte_gpa)
        .map(Pte::from);
    let mut pte;
    loop {
        pte = pte_access.map_err(|_| Error::GpaUnmapped(level))?;
        let large_page_supported = match page_table.level {
            3 => false,
            2 => page_table.page_shift > 12,
            _ => true,
        };
        if !pte.valid() || (!pte.not_large_page() && !large_page_supported) {
            return Err(Error::PageNotPresent(level));
        }
        let next_address = pte.pfn() << HV_PAGE_SHIFT;
        if pte.reserved_must_be_zero() != 0
            || (next_address & context.output_size_mask) != next_address
        {
            return Err(Error::InvalidPageTableFlags(level));
        }
        if page_table.level > 0 && pte.not_large_page() {
            if page_table.is_hierarchical_permissions {
                *is_user_address = *is_user_address && !pte.ap_table_privileged_only();
                *is_writeable_address = *is_writeable_address && !pte.ap_table_read_only();
                *is_executable_address = *is_executable_address
                    && if context.check_user_access {
                        pte.uxn_table()
                    } else {
                        pte.pxn_table()
                    };
            }
        } else {
            // check permissions
            *is_user_address = *is_user_address && pte.ap_unprivileged();
            *is_writeable_address = *is_writeable_address && !pte.ap_read_only();
            *is_executable_address = *is_executable_address
                && !(if context.check_user_access {
                    pte.user_no_execute()
                } else {
                    pte.privilege_no_execute()
                });
            if context.check_user_access {
                if context.flags.validate_read && !*is_user_address {
                    return Err(Error::PrivilegeViolation(level));
                }
                if context.write_no_execute && *is_writeable_address && *is_user_address {
                    *is_executable_address = false;
                }
            } else {
                if context.check_supervisor_access && *is_user_address {
                    return Err(Error::PrivilegeViolation(level));
                }
                if *is_writeable_address && (*is_user_address || context.write_no_execute) {
                    *is_executable_address = false;
                }
            }
            if context.flags.validate_write && !*is_writeable_address
                || context.flags.validate_execute && !*is_executable_address
            {
                return Err(Error::PrivilegeViolation(level));
            }
        }

        // Update access and dirty bits.
        let mut new_pte = pte;
        if context.flags.set_page_table_bits {
            new_pte.set_access_flag(true);
            if context.flags.validate_write && new_pte.dbm() {
                new_pte.set_ap_read_only(false);
            }
        }

        // Access bits already set.
        if new_pte == pte {
            break;
        }

        let r = if !pte.not_large_page() {
            context.guest_memory.compare_exchange(address, pte, new_pte)
        } else {
            context
                .guest_memory
                .compare_exchange(address, u64::from(pte) as u32, u64::from(new_pte) as u32)
                .map(|r| {
                    r.map(|n| Pte::from(n as u64))
                        .map_err(|n| Pte::from(n as u64))
                })
        };

        match r {
            Ok(Ok(_)) => {
                // Compare exchange succeeded, so continue.
                break;
            }
            Ok(Err(pte)) => {
                // Compare exchange failed. Loop around again.
                pte_access = Ok(pte);
                continue;
            }
            Err(err) => {
                // Memory access failed. Loop around again to handle the
                // failure consistently.
                pte_access = Err(err);
                continue;
            }
        }
    }
    let pfn_mask = !(1_u64 << (page_table.page_shift - HV_PAGE_SHIFT)).wrapping_sub(1);
    let next_address = (pte.pfn() & pfn_mask) << HV_PAGE_SHIFT;
    if page_table.level == 0 || !pte.not_large_page() {
        Ok(PageTableWalkResult::BaseGpa(
            next_address,
            (1 << page_table.span_shift) - 1,
        ))
    } else {
        Ok(PageTableWalkResult::Table(Aarch64PageTable {
            table_address_gpa: next_address,
            page_shift: page_table.page_shift,
            span_shift: page_table.span_shift - (page_table.page_shift - 3),
            level: page_table.level - 1,
            level_width: page_table.page_shift - 3,
            is_hierarchical_permissions: page_table.is_hierarchical_permissions,
        }))
    }
}

/// Translate a GVA by walking the processor's page tables.
pub fn translate_gva_to_gpa(
    guest_memory: &GuestMemory,
    gva: u64,
    registers: &TranslationRegisters,
    flags: TranslateFlags,
) -> Result<u64, Error> {
    tracing::trace!(gva, ?registers, ?flags, "translating gva");

    // If paging is disabled, just return the GVA as the GPA.
    if !registers.sctlr.m() {
        return Ok(gva);
    }

    // FEAT_LPA2 - Larger physical address for 4KB and 16KB translation granules

    let (check_user_access, check_supervisor_access) = match flags.privilege_check {
        TranslatePrivilegeCheck::None => (false, false),
        TranslatePrivilegeCheck::User => (true, false),
        TranslatePrivilegeCheck::Both => (true, true),
        TranslatePrivilegeCheck::CurrentPrivilegeLevel if registers.cpsr.el() == 0 => (true, false),
        TranslatePrivilegeCheck::Supervisor | TranslatePrivilegeCheck::CurrentPrivilegeLevel => {
            (false, true)
        }
    };
    let output_size = match registers.tcr.ips() {
        IntermPhysAddrSize::IPA_32_BITS_4_GB => 32,
        IntermPhysAddrSize::IPA_36_BITS_64_GB => 36,
        IntermPhysAddrSize::IPA_40_BITS_1_TB => 40,
        IntermPhysAddrSize::IPA_42_BITS_4_TB => 42,
        IntermPhysAddrSize::IPA_44_BITS_16_TB => 44,
        IntermPhysAddrSize::IPA_48_BITS_256_TB => 48,
        IntermPhysAddrSize::IPA_52_BITS_4_PB => 52,
        IntermPhysAddrSize::IPA_56_BITS_64_PB => 56,
        _ => return Err(Error::InvalidPageTableFlags(0)),
    };
    let write_no_execute = registers.sctlr.wxn();
    let walk_context = PageTableWalkContext {
        guest_memory,
        flags,
        check_user_access,
        check_supervisor_access,
        write_no_execute,
        output_size_mask: (1 << output_size) - 1,
    };
    let mut is_user_address = false;
    let mut is_writeable_address = true;
    let mut is_executable_address = true;
    let (address, mut page_table) = get_root_page_table(gva, registers, &walk_context.flags)?;
    let mut level = 1;
    loop {
        page_table = match get_next_page_table(
            level,
            address,
            &page_table,
            &walk_context,
            &mut is_user_address,
            &mut is_writeable_address,
            &mut is_executable_address,
        )? {
            PageTableWalkResult::BaseGpa(base_address, mask) => {
                break Ok(base_address + (gva & mask))
            }
            PageTableWalkResult::Table(next_table) => next_table,
        };
        level += 1;
    }
}
