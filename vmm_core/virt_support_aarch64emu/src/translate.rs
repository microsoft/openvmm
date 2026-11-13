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
use hvdef::HV_PAGE_SHIFT;
use hvdef::hypercall::TranslateGvaControlFlagsArm64;
use hvdef::hypercall::TranslateGvaResultCode;
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
    /// Override PSTATE.PAN (false clears, true sets).
    pub pan: Option<bool>,
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
            privilege_check: if flags.user_access() {
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
            pan: if flags.pan_clear() {
                Some(false)
            } else if flags.pan_set() {
                Some(true)
            } else {
                None
            },
        }
    }
}

/// Translation error.
#[derive(Debug, Error)]
pub enum Error {
    /// The walker cannot represent this translation regime.
    #[error("unsupported translation configuration: {0}")]
    Unsupported(&'static str),
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
    /// Access flag is clear and hardware access-flag management is disabled.
    #[error("access flag fault at level {0}")]
    AccessFlagFault(u8),
}

impl From<&Error> for TranslateGvaResultCode {
    fn from(err: &Error) -> TranslateGvaResultCode {
        match err {
            Error::Unsupported(_) => TranslateGvaResultCode::INTERCEPT,
            Error::InvalidAddressSize(_) => TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
            Error::InvalidPageTableFlags(_) => TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
            Error::GpaUnmapped(_) => TranslateGvaResultCode::GPA_UNMAPPED,
            Error::PageNotPresent(_) => TranslateGvaResultCode::PAGE_NOT_PRESENT,
            Error::PrivilegeViolation(_) => TranslateGvaResultCode::PRIVILEGE_VIOLATION,
            Error::AccessFlagFault(_) => TranslateGvaResultCode::PRIVILEGE_VIOLATION,
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
            Error::Unsupported(_) => {
                return EsrEl2::new().with_ec(aarch64defs::ExceptionClass::UNKNOWN.0);
            }
            Error::InvalidAddressSize(i) => FaultStatusCode::ADDRESS_SIZE_FAULT_LEVEL0.0 + i,
            Error::InvalidPageTableFlags(i) => FaultStatusCode::TRANSLATION_FAULT_LEVEL0.0 + i,
            Error::GpaUnmapped(i) => FaultStatusCode::SEA_TTW_LEVEL0.0 + i,
            Error::PageNotPresent(i) => FaultStatusCode::TRANSLATION_FAULT_LEVEL0.0 + i,
            Error::PrivilegeViolation(i) => FaultStatusCode::PERMISSION_FAULT_LEVEL0.0 + i,
            Error::AccessFlagFault(i) => FaultStatusCode::ACCESS_FLAG_FAULT_LEVEL0.0 + i,
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
        validate_read: mode != TranslateMode::Execute,
        validate_write: mode.is_write(),
        privilege_check: if matches!(
            mode,
            TranslateMode::ReadUnprivileged | TranslateMode::WriteUnprivileged
        ) {
            TranslatePrivilegeCheck::User
        } else {
            TranslatePrivilegeCheck::CurrentPrivilegeLevel
        },
        set_page_table_bits: true,
        pan: None,
    };

    let r = match translate_gva_to_gpa(support.guest_memory(), gva, &registers, flags) {
        Ok(gpa) => Ok(EmuTranslateResult {
            gpa,
            overlay_page: None,
        }),
        Err(err) => {
            if matches!(err, Error::Unsupported(_)) {
                return Ok(Err(EmuTranslateError {
                    code: err.into(),
                    event_info: None,
                }));
            }
            let mut syndrome: EsrEl2 = (&err).into();
            syndrome.set_il(true);
            if mode == TranslateMode::Execute {
                syndrome.set_ec(aarch64defs::ExceptionClass::INSTRUCTION_ABORT.0);
            } else {
                syndrome.set_wnr(mode.is_write());
            }
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
    let use_ttbr1 = (gva & (1 << 55)) != 0;
    if if use_ttbr1 {
        registers.tcr.epd1() != 0
    } else {
        registers.tcr.epd0() != 0
    } {
        return Err(Error::PageNotPresent(0));
    }
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
            registers.tcr.hpd1() == 0,
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
            registers.tcr.hpd0() == 0,
        )
    };
    if (49..=52).contains(&address_width) {
        return Err(Error::Unsupported("input addresses wider than 48 bits"));
    }
    if !(25..=48).contains(&address_width) {
        tracing::trace!(address_width, "Invalid TCR value");
        return Err(Error::PageNotPresent(0));
    }
    let num_levels = (address_width - granule_width).div_ceil(granule_width - 3);
    if num_levels == 0 || num_levels > 4 {
        tracing::trace!(address_width, granule_width, "Invalid page hierarchy");
        return Err(Error::InvalidPageTableFlags(0));
    }
    let ignore_top_byte =
        ignore_top_byte && (!flags.validate_execute || !ignore_top_byte_instruction);
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
        return Err(Error::PageNotPresent(0));
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
    pan: bool,
    hardware_access: bool,
    hardware_dirty: bool,
    vtom: Option<u64>,
    big_endian: bool,
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
        return Err(Error::InvalidAddressSize(0));
    }
    let index_mask = (1 << page_table.level_width) - 1;
    let pte_index = (address >> page_table.span_shift) & index_mask;
    let pte_gpa = page_table.table_address_gpa + (pte_index << 3);
    if context.vtom.is_some_and(|vtom| pte_gpa >= vtom) {
        return Err(Error::GpaUnmapped(level));
    }
    let mut pte_access = context.guest_memory.read_plain::<u64>(pte_gpa).map(|raw| {
        Pte::from(if context.big_endian {
            raw.swap_bytes()
        } else {
            raw
        })
    });
    let mut pte;
    loop {
        pte = pte_access.map_err(|_| Error::GpaUnmapped(level))?;
        let large_page_supported = match level {
            1 => page_table.page_shift == 12,
            2 => true,
            _ => false,
        };
        if !pte.valid() || (!pte.not_large_page() && !large_page_supported) {
            return Err(Error::PageNotPresent(level));
        }
        let next_address = pte.pfn() << HV_PAGE_SHIFT;
        let leaf = page_table.level == 0 || !pte.not_large_page();
        let reserved = pte.reserved_must_be_zero() & if leaf { 0b011 } else { 0b111 };
        if reserved != 0 {
            return Err(Error::InvalidPageTableFlags(level));
        }
        if (next_address & context.output_size_mask) != next_address {
            return Err(Error::InvalidAddressSize(level));
        }
        let alignment = if leaf {
            page_table.span_shift
        } else {
            page_table.page_shift
        };
        if next_address & ((1 << alignment) - 1) != 0 {
            return Err(Error::InvalidPageTableFlags(level));
        }
        // A failed compare-exchange must not retain restrictions from a stale PTE.
        let mut user = *is_user_address;
        let mut writable = *is_writeable_address;
        let mut executable = *is_executable_address;
        if page_table.level > 0 && pte.not_large_page() {
            if page_table.is_hierarchical_permissions {
                user &= !pte.ap_table_privileged_only();
                writable &= !pte.ap_table_read_only();
                executable &= !(context.check_user_access && pte.uxn_table()
                    || context.check_supervisor_access && pte.pxn_table());
            }
        } else {
            // check permissions
            user &= pte.ap_unprivileged();
            writable &= !pte.ap_read_only() || context.hardware_dirty && pte.dbm();
            executable &= !(context.check_user_access && pte.user_no_execute()
                || context.check_supervisor_access && pte.privilege_no_execute());
            if context.check_user_access {
                if (context.flags.validate_read
                    || context.flags.validate_write
                    || context.flags.validate_execute)
                    && !user
                {
                    return Err(Error::PrivilegeViolation(level));
                }
                if context.write_no_execute && writable && user {
                    executable = false;
                }
            }
            if context.check_supervisor_access {
                if context.pan
                    && user
                    && (context.flags.validate_read || context.flags.validate_write)
                {
                    return Err(Error::PrivilegeViolation(level));
                }
                if writable && (user || context.write_no_execute) {
                    executable = false;
                }
            }
            if context.flags.validate_write && !writable
                || context.flags.validate_execute && !executable
            {
                return Err(Error::PrivilegeViolation(level));
            }
            if context.flags.validate_execute
                && context.vtom.is_some_and(|vtom| next_address >= vtom)
            {
                return Err(Error::PrivilegeViolation(level));
            }
            if !pte.access_flag()
                && !context.hardware_access
                && (context.flags.validate_read
                    || context.flags.validate_write
                    || context.flags.validate_execute
                    || context.flags.set_page_table_bits)
            {
                return Err(Error::AccessFlagFault(level));
            }
        }

        // Update access and dirty bits.
        let mut new_pte = pte;
        if context.flags.set_page_table_bits && leaf {
            new_pte.set_access_flag(true);
            if context.flags.validate_write && context.hardware_dirty && new_pte.dbm() {
                new_pte.set_ap_read_only(false);
            }
        }

        // Access bits already set.
        if new_pte == pte {
            *is_user_address = user;
            *is_writeable_address = writable;
            *is_executable_address = executable;
            break;
        }

        let current = if context.big_endian {
            u64::from(pte).swap_bytes()
        } else {
            pte.into()
        };
        let new = if context.big_endian {
            u64::from(new_pte).swap_bytes()
        } else {
            new_pte.into()
        };
        let r = context
            .guest_memory
            .compare_exchange(pte_gpa, current, new)
            .map(|r| {
                r.map(|raw| {
                    Pte::from(if context.big_endian {
                        raw.swap_bytes()
                    } else {
                        raw
                    })
                })
                .map_err(|raw| {
                    Pte::from(if context.big_endian {
                        raw.swap_bytes()
                    } else {
                        raw
                    })
                })
            });

        match r {
            Ok(Ok(_)) => {
                *is_user_address = user;
                *is_writeable_address = writable;
                *is_executable_address = executable;
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
    let next_address = pte.pfn() << HV_PAGE_SHIFT;
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

    // This walker uses the pre-LPA descriptor format.
    if registers.tcr.ds() != 0 {
        return Err(Error::Unsupported("FEAT_LPA2 descriptors"));
    }

    let (address, mut page_table) = get_root_page_table(gva, registers, &flags)?;
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
        // With DS=0, IPS=0b110 selects 48-bit output for 4KB/16KB granules.
        IntermPhysAddrSize::IPA_52_BITS_4_PB if page_table.page_shift != 16 => 48,
        IntermPhysAddrSize::IPA_52_BITS_4_PB | IntermPhysAddrSize::IPA_56_BITS_64_PB => {
            return Err(Error::Unsupported("output addresses wider than 48 bits"));
        }
        _ => return Err(Error::InvalidPageTableFlags(0)),
    };
    let write_no_execute = registers.sctlr.wxn();
    let pan = flags.pan.unwrap_or(registers.cpsr.pan());
    let walk_context = PageTableWalkContext {
        guest_memory,
        flags,
        check_user_access,
        check_supervisor_access,
        write_no_execute,
        output_size_mask: (1 << output_size) - 1,
        pan,
        hardware_access: registers.tcr.ha() != 0,
        hardware_dirty: registers.tcr.ha() != 0 && registers.tcr.hd() != 0,
        vtom: match registers.encryption_mode {
            EncryptionMode::None => None,
            EncryptionMode::Vtom(vtom) => Some(vtom),
        },
        big_endian: registers.sctlr.ee(),
    };
    let mut is_user_address = true;
    let mut is_writeable_address = true;
    let mut is_executable_address = true;
    loop {
        let level = (3 - page_table.level) as u8;
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
                break Ok(base_address + (gva & mask));
            }
            PageTableWalkResult::Table(next_table) => next_table,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aarch64defs::Cpsr64;
    use aarch64defs::IntermPhysAddrSize;
    use aarch64defs::Pte;
    use aarch64defs::SctlrEl1;
    use aarch64defs::TranslationControlEl1;
    use guestmem::GuestMemory;
    use test_with_tracing::test;

    fn flags(mode: TranslateMode, privilege_check: TranslatePrivilegeCheck) -> TranslateFlags {
        TranslateFlags {
            validate_read: mode == TranslateMode::Read,
            validate_write: mode == TranslateMode::Write,
            validate_execute: mode == TranslateMode::Execute,
            privilege_check,
            set_page_table_bits: true,
            pan: None,
        }
    }

    fn mapping(
        shift: u64,
        width: u64,
        upper: bool,
    ) -> (GuestMemory, TranslationRegisters, u64, u64, Vec<(u64, Pte)>) {
        let gm = GuestMemory::allocate((6 << shift) as usize);
        let levels = (width - shift).div_ceil(shift - 3);
        let low = 0x12_3456u64 & ((1 << width) - 1);
        let gva = if upper {
            !((1 << width) - 1) | low
        } else {
            low
        };
        let mut registers = make_registers();
        registers.tcr.set_t0sz(64 - width);
        registers.tcr.set_t1sz(64 - width);
        registers.tcr.set_ha(1);
        registers.tcr.set_tg0(match shift {
            12 => TranslationGranule0::TG_4KB,
            14 => TranslationGranule0::TG_16KB,
            16 => TranslationGranule0::TG_64KB,
            _ => unreachable!(),
        });
        registers.tcr.set_tg1(match shift {
            12 => TranslationGranule1::TG_4KB,
            14 => TranslationGranule1::TG_16KB,
            16 => TranslationGranule1::TG_64KB,
            _ => unreachable!(),
        });
        registers.ttbr0 = 1 << shift;
        registers.ttbr1 = 1 << shift;
        let mut entries = Vec::new();
        for i in 0..levels {
            let span = shift + (shift - 3) * (levels - i - 1);
            let index_width = if i == 0 { width - span } else { shift - 3 };
            let address = ((i + 1) << shift) + (((gva >> span) & ((1 << index_width) - 1)) * 8);
            let output = if i + 1 == levels {
                0x1000_0000
            } else {
                (i + 2) << shift
            };
            let pte = Pte::new()
                .with_valid(true)
                .with_not_large_page(true)
                .with_ap_unprivileged(i + 1 == levels)
                .with_pfn(output >> 12);
            gm.write_plain(address, &pte).unwrap();
            entries.push((address, pte));
        }
        let leaf = entries.last().unwrap().0;
        (gm, registers, gva, leaf, entries)
    }

    #[test]
    fn all_granules_and_root_widths_update_only_leaf_descriptors() {
        for shift in [12, 14, 16] {
            for width in [25, 30, 36, 39, 48] {
                for upper in [false, true] {
                    let (gm, registers, gva, leaf, entries) = mapping(shift, width, upper);
                    let result = translate_gva_to_gpa(
                        &gm,
                        gva,
                        &registers,
                        flags(TranslateMode::Read, TranslatePrivilegeCheck::User),
                    )
                    .unwrap();
                    assert_eq!(
                        result,
                        0x1000_0000 | (gva & ((1 << shift) - 1)),
                        "shift={shift}, width={width}, upper={upper}"
                    );
                    for (address, original) in entries {
                        let updated: Pte = gm.read_plain(address).unwrap();
                        assert_eq!(
                            updated,
                            if address == leaf {
                                original.with_access_flag(true)
                            } else {
                                original
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn hardware_access_and_dirty_updates_respect_tcr() {
        for ha in [0, 1] {
            for hd in [0, 1] {
                let (gm, mut registers, gva, leaf, _) = mapping(12, 30, false);
                registers.tcr.set_ha(ha);
                registers.tcr.set_hd(hd);
                let pte: Pte = gm.read_plain(leaf).unwrap();
                let pte = pte
                    .with_dbm(true)
                    .with_ap_read_only(true)
                    .with_user_no_execute(true);
                gm.write_plain(leaf, &pte).unwrap();
                let read = translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Read, TranslatePrivilegeCheck::User),
                );
                if ha == 0 {
                    assert!(matches!(read, Err(Error::AccessFlagFault(3))));
                } else {
                    assert!(read.is_ok());
                }
                let write = translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Write, TranslatePrivilegeCheck::User),
                );
                if ha == 1 && hd == 1 {
                    assert!(write.is_ok());
                    let updated: Pte = gm.read_plain(leaf).unwrap();
                    assert!(updated.access_flag());
                    assert!(!updated.ap_read_only());
                    assert!(updated.user_no_execute());
                    assert!(updated.dbm());
                } else {
                    assert!(matches!(write, Err(Error::PrivilegeViolation(3))));
                }
            }
        }
    }

    #[test]
    fn user_write_and_supervisor_pan_permissions() {
        let (gm, mut registers, gva, leaf, _) = mapping(12, 30, false);
        let pte: Pte = gm.read_plain(leaf).unwrap();
        gm.write_plain(leaf, &pte.with_ap_unprivileged(false))
            .unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Write, TranslatePrivilegeCheck::User)
            ),
            Err(Error::PrivilegeViolation(3)),
        ));
        gm.write_plain(leaf, &pte).unwrap();
        registers.cpsr.set_el(1);
        for pan in [false, true] {
            registers.cpsr.set_pan(pan);
            for mode in [TranslateMode::Read, TranslateMode::Write] {
                let result = translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(mode, TranslatePrivilegeCheck::CurrentPrivilegeLevel),
                );
                assert_eq!(result.is_err(), pan);
            }
        }
    }

    #[test]
    fn pan_override_does_not_override_el0_permissions() {
        let (gm, mut registers, gva, leaf, _) = mapping(12, 30, false);
        registers.cpsr.set_el(1);
        registers.cpsr.set_pan(true);
        let hvflags = TranslateGvaControlFlagsArm64::new()
            .with_validate_read(true)
            .with_pan_clear(true);
        assert!(
            translate_gva_to_gpa(&gm, gva, &registers, TranslateFlags::from_hv_flags(hvflags))
                .is_ok()
        );
        let pte: Pte = gm.read_plain(leaf).unwrap();
        gm.write_plain(leaf, &pte.with_ap_unprivileged(false))
            .unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                TranslateFlags::from_hv_flags(hvflags.with_user_access(true))
            ),
            Err(Error::PrivilegeViolation(3)),
        ));
    }

    #[test]
    fn big_endian_tables_preserve_descriptor_bytes_on_updates() {
        let (gm, mut registers, gva, leaf, entries) = mapping(12, 30, false);
        registers.sctlr.set_ee(true);
        for &(address, pte) in &entries {
            gm.write_at(address, &u64::from(pte).to_be_bytes()).unwrap();
        }
        assert_eq!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            )
            .unwrap(),
            0x1000_0456,
        );
        for (address, pte) in entries {
            let mut bytes = [0; 8];
            gm.read_at(address, &mut bytes).unwrap();
            let expected = if address == leaf {
                pte.with_access_flag(true)
            } else {
                pte
            };
            assert_eq!(u64::from_be_bytes(bytes), u64::from(expected));
        }
    }

    #[test]
    fn blocks_and_encrypted_table_boundaries_are_validated() {
        let (gm, mut registers, gva, _, entries) = mapping(12, 30, false);
        let (root, original) = entries[0];
        let block = Pte::new()
            .with_valid(true)
            .with_access_flag(true)
            .with_ap_unprivileged(true)
            .with_pfn(0x200);
        gm.write_plain(root, &block).unwrap();
        assert_eq!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            )
            .unwrap(),
            0x20_0000 | gva,
        );
        gm.write_plain(root, &block.with_pfn(0x201)).unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::InvalidPageTableFlags(2)),
        ));
        gm.write_plain(root, &original).unwrap();
        registers.encryption_mode = EncryptionMode::Vtom(0x1000);
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::GpaUnmapped(2)),
        ));
    }

    #[test]
    fn hierarchical_permissions_can_be_disabled() {
        for upper in [false, true] {
            for disabled in [0, 1] {
                let (gm, mut registers, gva, _, entries) = mapping(12, 30, upper);
                if upper {
                    registers.tcr.set_hpd1(disabled);
                } else {
                    registers.tcr.set_hpd0(disabled);
                }
                let (address, pte) = entries[0];
                gm.write_plain(
                    address,
                    &pte.with_ap_table_read_only(true).with_uxn_table(true),
                )
                .unwrap();
                for mode in [TranslateMode::Write, TranslateMode::Execute] {
                    let result = translate_gva_to_gpa(
                        &gm,
                        gva,
                        &registers,
                        flags(mode, TranslatePrivilegeCheck::User),
                    );
                    assert_eq!(result.is_err(), disabled == 0);
                }
            }
        }
    }

    #[test]
    fn tagged_instruction_addresses_obey_tbid() {
        for upper in [false, true] {
            let (gm, mut registers, gva, _, _) = mapping(12, 48, upper);
            let tagged = (gva & 0x00ff_ffff_ffff_ffff) | (0xab << 56);
            if upper {
                registers.tcr.set_tbi1(1);
            } else {
                registers.tcr.set_tbi0(1);
            }
            for tbid in [0, 1] {
                if upper {
                    registers.tcr.set_tbid1(tbid);
                } else {
                    registers.tcr.set_tbid0(tbid);
                }
                assert!(
                    translate_gva_to_gpa(
                        &gm,
                        tagged,
                        &registers,
                        flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                    )
                    .is_ok()
                );
                let result = translate_gva_to_gpa(
                    &gm,
                    tagged,
                    &registers,
                    flags(TranslateMode::Execute, TranslatePrivilegeCheck::User),
                );
                assert_eq!(result.is_err(), tbid == 1);
            }
            if upper {
                registers.tcr.set_epd1(1);
            } else {
                registers.tcr.set_epd0(1);
            }
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                ),
                Err(Error::PageNotPresent(0)),
            ));
        }
    }

    #[test]
    fn descriptor_faults_report_architectural_levels() {
        for (error, expected) in [
            (
                Error::PageNotPresent(3),
                FaultStatusCode::TRANSLATION_FAULT_LEVEL3,
            ),
            (
                Error::AccessFlagFault(2),
                FaultStatusCode::ACCESS_FLAG_FAULT_LEVEL2,
            ),
            (Error::GpaUnmapped(1), FaultStatusCode::SEA_TTW_LEVEL1),
            (
                Error::PrivilegeViolation(0),
                FaultStatusCode::PERMISSION_FAULT_LEVEL0,
            ),
        ] {
            assert_eq!(EsrEl2::from(error).lower_iss(), expected.0);
        }
        let (gm, registers, gva, leaf, _) = mapping(12, 30, false);
        let pte: Pte = gm.read_plain(leaf).unwrap();
        gm.write_plain(leaf, &pte.with_not_large_page(false))
            .unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::PageNotPresent(3)),
        ));
    }

    #[test]
    fn guarded_leaves_and_debug_translation_do_not_require_af() {
        let (gm, mut registers, gva, leaf, entries) = mapping(12, 30, false);
        registers.tcr.set_ha(0);
        let original: Pte = gm.read_plain(leaf).unwrap();
        let guarded = Pte::from(u64::from(original) | (1 << 50));
        gm.write_plain(leaf, &guarded).unwrap();
        let mut debug = flags(TranslateMode::Read, TranslatePrivilegeCheck::None);
        debug.validate_read = false;
        debug.set_page_table_bits = false;
        assert_eq!(
            translate_gva_to_gpa(&gm, gva, &registers, debug).unwrap(),
            0x1000_0456
        );
        assert_eq!(gm.read_plain::<Pte>(leaf).unwrap(), guarded);
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::AccessFlagFault(3)),
        ));
        gm.write_plain(leaf, &guarded.with_access_flag(true))
            .unwrap();
        assert_eq!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            )
            .unwrap(),
            0x1000_0456,
        );
        for bit in [48, 49] {
            gm.write_plain(leaf, &Pte::from(u64::from(original) | (1 << bit)))
                .unwrap();
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                ),
                Err(Error::InvalidPageTableFlags(3)),
            ));
        }
        gm.write_plain(leaf, &original).unwrap();
        let (root, table) = entries[0];
        gm.write_plain(root, &Pte::from(u64::from(table) | (1 << 50)))
            .unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::InvalidPageTableFlags(2)),
        ));
    }

    #[test]
    fn large_granules_reject_level_one_blocks() {
        for shift in [14, 16] {
            let (gm, registers, gva, _, entries) =
                mapping(shift, if shift == 16 { 48 } else { 39 }, false);
            let (root, original) = entries[0];
            assert_eq!(entries.len(), 3);
            let block = original
                .with_not_large_page(false)
                .with_pfn(0)
                .with_access_flag(true)
                .with_ap_unprivileged(true);
            gm.write_plain(root, &block).unwrap();
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                ),
                Err(Error::PageNotPresent(1)),
            ));
            gm.write_plain(root, &original).unwrap();
            let (level_two, descriptor) = entries[1];
            gm.write_plain(
                level_two,
                &descriptor
                    .with_not_large_page(false)
                    .with_pfn(0)
                    .with_access_flag(true)
                    .with_ap_unprivileged(true),
            )
            .unwrap();
            assert_eq!(
                translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                )
                .unwrap(),
                gva,
            );
        }
        let (gm, registers, gva, _, entries) = mapping(12, 39, false);
        let (root, _) = entries[0];
        let block = Pte::new()
            .with_valid(true)
            .with_access_flag(true)
            .with_ap_unprivileged(true)
            .with_pfn(0x40000);
        gm.write_plain(root, &block).unwrap();
        assert_eq!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            )
            .unwrap(),
            0x4000_0000 | gva,
        );
    }

    #[test]
    fn root_address_size_fault_is_level_zero_and_unsupported_is_distinct() {
        let (gm, mut registers, gva, _, _) = mapping(12, 30, false);
        registers.tcr.set_ips(IntermPhysAddrSize::IPA_32_BITS_4_GB);
        registers.ttbr0 = 1 << 32;
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::InvalidAddressSize(0)),
        ));
        registers.tcr.set_ips(IntermPhysAddrSize::IPA_56_BITS_64_PB);
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::Unsupported(_)),
        ));
        assert_eq!(
            TranslateGvaResultCode::from(Error::Unsupported("test configuration")),
            TranslateGvaResultCode::INTERCEPT
        );
        let (gm, mut registers, gva, _, entries) = mapping(12, 39, false);
        registers.tcr.set_ips(IntermPhysAddrSize::IPA_32_BITS_4_GB);
        let (root, table) = entries[0];
        gm.write_plain(root, &table.with_pfn(1 << 20)).unwrap();
        assert!(matches!(
            translate_gva_to_gpa(
                &gm,
                gva,
                &registers,
                flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
            ),
            Err(Error::InvalidAddressSize(1)),
        ));
    }

    #[test]
    fn ips_52_uses_the_selected_granules_effective_output_size() {
        for upper in [false, true] {
            for shift in [12, 14, 16] {
                let (gm, mut registers, gva, leaf, _) = mapping(shift, 48, upper);
                registers.tcr.set_ips(IntermPhysAddrSize::IPA_52_BITS_4_PB);
                // The unselected granule must not determine the output width.
                if upper {
                    registers.tcr.set_tg0(TranslationGranule0::TG_64KB);
                } else {
                    registers.tcr.set_tg1(TranslationGranule1::TG_64KB);
                }
                let walk = || {
                    translate_gva_to_gpa(
                        &gm,
                        gva,
                        &registers,
                        flags(TranslateMode::Read, TranslatePrivilegeCheck::User),
                    )
                };
                if shift == 16 {
                    assert!(matches!(walk(), Err(Error::Unsupported(_))));
                    continue;
                }
                assert_eq!(walk().unwrap(), 0x1000_0000 | (gva & ((1 << shift) - 1)));
                let original: Pte = gm.read_plain(leaf).unwrap();
                let last_output = (1u64 << 48) - (1 << shift);
                gm.write_plain(leaf, &original.with_pfn(last_output >> HV_PAGE_SHIFT))
                    .unwrap();
                assert_eq!(walk().unwrap(), last_output | (gva & ((1 << shift) - 1)));
                gm.write_plain(leaf, &original).unwrap();
                registers.tcr.set_ds(1);
                assert!(matches!(
                    translate_gva_to_gpa(
                        &gm,
                        gva,
                        &registers,
                        flags(TranslateMode::Read, TranslatePrivilegeCheck::User)
                    ),
                    Err(Error::Unsupported(_)),
                ));
            }
        }
    }

    /// Helper: set up a 2-level page table walk for a 4KB granule.
    ///
    /// Memory layout (all within first 16 KB of guest memory):
    ///   GPA 0x0000: Level 1 table (root)
    ///   GPA 0x1000: Level 0 table
    ///   Output maps to GPA 0x2000
    ///
    /// TCR: t0sz = 34 → address_width = 30 → 2 levels (L1 → L0)
    fn setup_page_tables(
        gm: &GuestMemory,
        l1_table_pte: Pte, // table descriptor at L1[0]
        l0_leaf_pte: Pte,  // leaf PTE at L0[0]
    ) {
        gm.write_plain::<u64>(0x0000, &l1_table_pte.into()).unwrap();
        gm.write_plain::<u64>(0x1000, &l0_leaf_pte.into()).unwrap();
    }

    fn make_registers() -> TranslationRegisters {
        TranslationRegisters {
            cpsr: Cpsr64::new(),                 // EL0
            sctlr: SctlrEl1::new().with_m(true), // paging enabled
            tcr: TranslationControlEl1::new()
                .with_t0sz(34) // address_width = 30 → 2 levels
                .with_ips(IntermPhysAddrSize::IPA_48_BITS_256_TB),
            // tg0 = 0 → TranslationGranule0::TG_4KB
            ttbr0: 0x0000, // L1 table at GPA 0
            ttbr1: 0,
            syndrome: 0,
            encryption_mode: EncryptionMode::None,
        }
    }

    #[test]
    fn test_table_level_uxn_should_deny_execution() {
        let gm = GuestMemory::allocate(0x4000);

        // L1 table descriptor: valid, points to L0 table at 0x1000,
        // with UXN_TABLE = 1 (user execute never).
        let l1_pte = Pte::new()
            .with_valid(true)
            .with_not_large_page(true) // table descriptor
            .with_pfn(0x1000 >> 12)
            .with_uxn_table(true); // USER EXECUTE NEVER

        // L0 leaf PTE: valid page at 0x2000, user accessible, executable
        // at the leaf level (user_no_execute = false).
        let l0_pte = Pte::new()
            .with_not_large_page(true)
            .with_valid(true)
            .with_pfn(0x2000 >> 12)
            .with_access_flag(true)
            .with_ap_unprivileged(true); // user accessible
        // user_no_execute defaults to false → leaf says executable

        setup_page_tables(&gm, l1_pte, l0_pte);

        let registers = make_registers();
        // HPD=0 means hierarchical permissions ARE enabled.
        assert_eq!(registers.tcr.hpd0(), 0);

        let flags = TranslateFlags {
            validate_execute: true,
            validate_read: true,
            validate_write: false,
            privilege_check: TranslatePrivilegeCheck::User,
            set_page_table_bits: false,
            pan: None,
        };

        let result = translate_gva_to_gpa(&gm, 0, &registers, flags);

        // The table descriptor says UXN=1 (user execute never).
        // Translation with validate_execute should FAIL.
        assert!(matches!(result, Err(Error::PrivilegeViolation(_))));
    }

    #[test]
    fn test_user_read_succeeds_on_user_accessible_page() {
        let gm = GuestMemory::allocate(0x4000);

        // L1 table descriptor: plain table, no restrictions.
        let l1_pte = Pte::new()
            .with_valid(true)
            .with_not_large_page(true)
            .with_pfn(0x1000 >> 12);

        // L0 leaf PTE: user-accessible page at GPA 0x2000.
        let l0_pte = Pte::new()
            .with_not_large_page(true)
            .with_valid(true)
            .with_pfn(0x2000 >> 12)
            .with_access_flag(true)
            .with_ap_unprivileged(true); // USER ACCESSIBLE

        setup_page_tables(&gm, l1_pte, l0_pte);

        let registers = make_registers();

        // User-mode read validation.
        let flags = TranslateFlags {
            validate_execute: false,
            validate_read: true,
            validate_write: false,
            privilege_check: TranslatePrivilegeCheck::User,
            set_page_table_bits: false,
            pan: None,
        };

        let result = translate_gva_to_gpa(&gm, 0, &registers, flags);

        // The page has ap_unprivileged=true, so a user-mode read should
        // succeed and resolve to GPA 0x2000.
        assert_eq!(result.unwrap(), 0x2000);
    }

    #[test]
    fn test_table_level_pxn_should_deny_execution() {
        let gm = GuestMemory::allocate(0x4000);

        // L1 table descriptor with PXN_TABLE = 1 (privileged execute never).
        let l1_pte = Pte::new()
            .with_valid(true)
            .with_not_large_page(true)
            .with_pfn(0x1000 >> 12)
            .with_pxn_table(true); // PRIVILEGED EXECUTE NEVER

        // L0 leaf PTE: valid, executable at leaf level.
        let l0_pte = Pte::new()
            .with_not_large_page(true)
            .with_valid(true)
            .with_pfn(0x2000 >> 12)
            .with_access_flag(true);
        // privilege_no_execute defaults to false → leaf says executable

        setup_page_tables(&gm, l1_pte, l0_pte);

        let registers = make_registers();

        let flags = TranslateFlags {
            validate_execute: true,
            validate_read: true,
            validate_write: false,
            privilege_check: TranslatePrivilegeCheck::Supervisor,
            set_page_table_bits: false,
            pan: None,
        };

        let result = translate_gva_to_gpa(&gm, 0, &registers, flags);

        // The table descriptor says PXN=1 (privileged execute never).
        // Translation with validate_execute should FAIL.
        assert!(matches!(result, Err(Error::PrivilegeViolation(_))));
    }
}
