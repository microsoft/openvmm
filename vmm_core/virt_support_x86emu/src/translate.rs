// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86-64 page table walking.

#![warn(missing_docs)]

use guestmem::GuestMemory;
use hvdef::hypercall::TranslateGvaControlFlagsX64;
use hvdef::hypercall::TranslateGvaResultCode;
use thiserror::Error;
use x86defs::LargePde;
use x86defs::Pte;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use x86defs::X64_CR0_PG;
use x86defs::X64_CR0_WP;
use x86defs::X64_CR4_LA57;
use x86defs::X64_CR4_PAE;
use x86defs::X64_CR4_PSE;
use x86defs::X64_CR4_SMAP;
use x86defs::X64_CR4_SMEP;
use x86defs::X64_EFER_LMA;
use x86defs::X64_EFER_NXE;
use x86defs::is_canonical_address;

/// Registers needed to walk the page table.
#[derive(Debug, Clone)]
pub struct TranslationRegisters {
    /// CR0
    pub cr0: u64,
    /// CR4
    pub cr4: u64,
    /// EFER
    pub efer: u64,
    /// CR3
    pub cr3: u64,
    /// RFLAGS
    pub rflags: u64,
    /// SS
    pub ss: SegmentRegister,
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
    /// Allow access even if SMAP would prevent it.
    pub override_smap: bool,
    /// Enforce SMAP even if it is disabled via the AC flag.
    pub enforce_smap: bool,
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
    pub fn from_hv_flags(flags: TranslateGvaControlFlagsX64) -> Self {
        Self {
            validate_execute: flags.validate_execute(),
            validate_read: flags.validate_read(),
            validate_write: flags.validate_write(),
            override_smap: flags.override_smap(),
            enforce_smap: flags.enforce_smap(),
            privilege_check: if flags.privilege_exempt() {
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

/// Result of translation
pub struct TranslateResult {
    /// The translated GPA.
    pub gpa: u64,

    /// Information from the walk that can be used to determine memory type
    pub cache_info: TranslateCachingInfo,
}

/// Information from a translation walk that can be used to determine memory
/// type.
pub enum TranslateCachingInfo {
    /// Paging wasn't enabled for the translation.
    NoPaging,
    /// State from a page table walk
    Paging {
        /// Index that can be used into the pat register to determine cache type
        pat_index: u64,
    },
}

/// Translation error.
#[derive(Debug, Error)]
pub enum Error {
    /// The page table flags were invalid.
    #[error("invalid page table flags")]
    InvalidPageTableFlags,
    /// The requested GVA is a non-canonical address.
    #[error("non-canonical address")]
    NonCanonicalAddress,
    /// A page table GPA was not mapped.
    #[error("gpa unmapped")]
    GpaUnmapped,
    /// The page was not present in the page table.
    #[error("page not present")]
    PageNotPresent,
    /// Accessing the GVA would create a privilege violation.
    #[error("privilege violation")]
    PrivilegeViolation,
}

impl From<Error> for TranslateGvaResultCode {
    fn from(err: Error) -> TranslateGvaResultCode {
        match err {
            Error::InvalidPageTableFlags | Error::NonCanonicalAddress => {
                TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS
            }
            Error::GpaUnmapped => TranslateGvaResultCode::GPA_UNMAPPED,
            Error::PageNotPresent => TranslateGvaResultCode::PAGE_NOT_PRESENT,
            Error::PrivilegeViolation => TranslateGvaResultCode::PRIVILEGE_VIOLATION,
        }
    }
}

/// Translate a GVA by walking the processor's page tables.
pub fn translate_gva_to_gpa(
    guest_memory: &GuestMemory,
    gva: u64,
    registers: &TranslationRegisters,
    flags: TranslateFlags,
) -> Result<TranslateResult, Error> {
    tracing::trace!(gva, ?registers, ?flags, "translating gva");

    let long_mode = registers.efer & X64_EFER_LMA != 0;
    // Truncate the address if operating in 32-bit mode.
    let gva = if long_mode { gva } else { gva as u32 as u64 };

    // If paging is disabled, just return the GVA as the GPA.
    if registers.cr0 & X64_CR0_PG == 0 {
        return Ok(TranslateResult {
            gpa: gva,
            cache_info: TranslateCachingInfo::NoPaging,
        });
    }

    let address_bits;
    let large_pte;
    if long_mode {
        large_pte = true;
        address_bits = if registers.cr4 & X64_CR4_LA57 != 0 {
            57
        } else {
            48
        };

        if !is_canonical_address(gva, address_bits) {
            return Err(Error::NonCanonicalAddress);
        }
    } else if registers.cr4 & X64_CR4_PAE != 0 {
        large_pte = true;
        // Only 32 bits are used from the input address; higher bits are zeroed
        // above. Bits 30..32 are used on x86 to index into the PDP table, but
        // for simplicity the code below uses the full 9-bit range 30..39.
        address_bits = 39;
    } else {
        large_pte = false;
        address_bits = 32;
    }

    let (user_access, supervisor_access) = match flags.privilege_check {
        TranslatePrivilegeCheck::None => (false, false),
        TranslatePrivilegeCheck::User => (true, false),
        TranslatePrivilegeCheck::Both => (true, true),
        TranslatePrivilegeCheck::CurrentPrivilegeLevel
            if registers.ss.attributes.descriptor_privilege_level() == 3 =>
        {
            (true, false)
        }
        TranslatePrivilegeCheck::Supervisor | TranslatePrivilegeCheck::CurrentPrivilegeLevel => {
            (false, true)
        }
    };

    let pae = !long_mode && large_pte;
    let check_write = flags.validate_write
        && (user_access || !supervisor_access || registers.cr0 & X64_CR0_WP != 0);
    let mut no_user_access = supervisor_access
        && ((flags.validate_execute && registers.cr4 & X64_CR4_SMEP != 0)
            || ((flags.validate_read || flags.validate_write)
                && !flags.override_smap
                && registers.cr4 & X64_CR4_SMAP != 0
                && (flags.enforce_smap || !RFlags::from(registers.rflags).alignment_check())));

    let mut gpa_base = if pae {
        registers.cr3 & 0xffff_ffe0
    } else {
        registers.cr3 & !0xfff
    };
    let mut remaining_bits: u32 = address_bits;
    let cache_disable: bool;
    let write_through: bool;
    let pat_supported: bool;
    loop {
        // Compute the PTE address.
        let pte_address = if large_pte {
            // Consume the next 9 bits as an index into the table.
            //
            // Note that for 32-bit with PAE, the PDP table is only 4 entries,
            // but the high 7 bits of the index (bits 32..39 of the address)
            // were zeroed above.
            remaining_bits -= 9;
            gpa_base + (((gva >> remaining_bits) & 0x1ff) * 8)
        } else {
            // Consume the next 10 bits as an index into the table.
            remaining_bits -= 10;
            gpa_base + (((gva >> remaining_bits) & 0x3ff) * 4)
        };

        // All PTE accesses occur to encrypted memory. If VTOM is enabled, then
        // just fail the translation in shared memory since there is no way to
        // set the c bit. In theory we could just mask off the VTOM bit to get
        // to an encrypted address, but that depends on the hypervisor aliasing
        // the memory identically across VTOM, which is not guaranteed at this
        // layer in the stack.
        let pte_address = match registers.encryption_mode {
            EncryptionMode::Vtom(vtom) => {
                if pte_address >= vtom {
                    return Err(Error::InvalidPageTableFlags);
                }
                pte_address
            }
            EncryptionMode::None => pte_address,
        };

        let mut pte_access = if large_pte {
            guest_memory.read_plain::<u64>(pte_address).map(Pte::from)
        } else {
            guest_memory
                .read_plain::<u32>(pte_address)
                .map(|n| Pte::from(n as u64))
        };

        // Loop on updating PTE a/d flags.
        let (pte, done) = loop {
            // TODO: different fault for VTL violation
            let pte = pte_access.map_err(|_| Error::GpaUnmapped)?;
            gpa_base = pte.pfn() << 12;

            if !pte.present() {
                tracing::trace!(pte_address, ?pte, "page not present");
                return Err(Error::PageNotPresent);
            }

            let pae_pdpte = pae && remaining_bits == 30;
            if pae_pdpte {
                // Legacy PAE PDPTEs have neither permissions nor A/D bits.
                if pte.available1() != 0 || pte.no_execute() || u64::from(pte) & 0x1e6 != 0 {
                    return Err(Error::InvalidPageTableFlags);
                }
            } else if (pae && pte.available1() != 0)
                || (large_pte && registers.efer & X64_EFER_NXE == 0 && pte.no_execute())
            {
                return Err(Error::InvalidPageTableFlags);
            }

            // Determine whether this is the terminal PTE.
            if large_pte && remaining_bits > 30 && pte.pat() {
                return Err(Error::InvalidPageTableFlags);
            }
            let done = remaining_bits == 12
                || (!pae_pdpte && (large_pte || registers.cr4 & X64_CR4_PSE != 0) && pte.pat());
            if done && large_pte && remaining_bits > 12 {
                let reserved_address_bits = ((1u64 << remaining_bits) - 1) & !0x1fff;
                if gpa_base & reserved_address_bits != 0 {
                    return Err(Error::InvalidPageTableFlags);
                }
            }

            if !pae_pdpte
                && ((check_write && !pte.read_write())
                    || (flags.validate_execute && pte.no_execute())
                    || (user_access && !pte.user()))
            {
                return Err(Error::PrivilegeViolation);
            }

            if done {
                if no_user_access && pte.user() {
                    return Err(Error::PrivilegeViolation);
                }

                // Only allow execute from encrypted memory.
                if flags.validate_execute {
                    let encrypted = match registers.encryption_mode {
                        EncryptionMode::Vtom(vtom) => gpa_base < vtom,
                        EncryptionMode::None => true,
                    };
                    if !encrypted {
                        return Err(Error::InvalidPageTableFlags);
                    }
                }
            }

            // Update access and dirty bits.
            let mut new_pte = pte;
            if flags.set_page_table_bits && !pae_pdpte {
                new_pte.set_accessed(true);
                if flags.validate_write && done {
                    new_pte.set_dirty(true);
                }
            }

            if new_pte != pte {
                let r = if large_pte {
                    guest_memory.compare_exchange(pte_address, pte, new_pte)
                } else {
                    guest_memory
                        .compare_exchange(
                            pte_address,
                            u64::from(pte) as u32,
                            u64::from(new_pte) as u32,
                        )
                        .map(|r| {
                            r.map(|n| Pte::from(n as u64))
                                .map_err(|n| Pte::from(n as u64))
                        })
                };

                match r {
                    Ok(Ok(_)) => {
                        // Compare exchange succeeded, so continue.
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

            break (pte, done);
        };

        // When user permission is revoked at any level of the hierarchy,
        // supervisor access will always be permitted regardless of the user bit
        // in the terminal PTE.
        if !(pae && remaining_bits == 30) && !pte.user() {
            no_user_access = false;
        }

        if done {
            cache_disable = pte.cache_disable();
            write_through = pte.write_through();
            pat_supported = if remaining_bits == 12 {
                pte.pat()
            } else {
                let large_pde = LargePde::from(u64::from(pte));
                large_pde.pat()
            };

            break;
        }
    }

    // The bits that didn't get used for page table indexes form the offset into
    // the page (of whatever size).
    let address_mask = !0 << remaining_bits;
    let pat_index =
        ((cache_disable as u64) << 1) | (write_through as u64) | ((pat_supported as u64) << 2);
    Ok(TranslateResult {
        gpa: (gpa_base & address_mask) | (gva & !address_mask),
        cache_info: TranslateCachingInfo::Paging { pat_index },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulate::TranslateMode;
    use test_with_tracing::test;
    use x86defs::is_canonical_address;

    fn translation_flags(mode: TranslateMode) -> TranslateFlags {
        TranslateFlags {
            validate_execute: mode == TranslateMode::Execute,
            validate_read: true,
            validate_write: mode == TranslateMode::Write,
            override_smap: false,
            enforce_smap: false,
            privilege_check: TranslatePrivilegeCheck::CurrentPrivilegeLevel,
            set_page_table_bits: true,
        }
    }

    fn pae_memory() -> (GuestMemory, TranslationRegisters) {
        let gm = GuestMemory::allocate(0x5000);
        let entry = Pte::new().with_present(true);
        gm.write_plain(0x1020, &entry.with_pfn(2)).unwrap();
        let entry = entry.with_read_write(true).with_user(true);
        gm.write_plain(0x2000, &entry.with_pfn(3)).unwrap();
        gm.write_plain(0x3008, &entry.with_pfn(4)).unwrap();
        let registers = TranslationRegisters {
            cr0: X64_CR0_PG | x86defs::X64_CR0_PE | X64_CR0_WP,
            cr4: X64_CR4_PAE,
            efer: X64_EFER_NXE,
            cr3: 0x1020,
            rflags: 0,
            ss: SegmentRegister {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: x86defs::SegmentAttributes::new().with_descriptor_privilege_level(3),
            },
            encryption_mode: EncryptionMode::None,
        };
        (gm, registers)
    }

    #[test]
    fn pae_translation_and_accessed_dirty_bits() {
        use crate::emulate::TranslateMode;

        for mode in [
            TranslateMode::Read,
            TranslateMode::Write,
            TranslateMode::Execute,
        ] {
            let (gm, registers) = pae_memory();
            let result =
                translate_gva_to_gpa(&gm, 0x1234, &registers, translation_flags(mode)).unwrap();
            assert_eq!(result.gpa, 0x4234);
            let pdpte: Pte = gm.read_plain(0x1020).unwrap();
            assert_eq!(u64::from(pdpte), 0x2001);
            let pde: Pte = gm.read_plain(0x2000).unwrap();
            let pte: Pte = gm.read_plain(0x3008).unwrap();
            assert!(pde.accessed());
            assert!(pte.accessed());
            assert!(!pde.dirty());
            assert_eq!(pte.dirty(), mode == TranslateMode::Write);
        }
    }

    #[test]
    fn supervisor_write_protection() {
        use crate::emulate::TranslateMode;

        for read_only_entry in [0x2000, 0x3008] {
            for user_mode in [false, true] {
                for write_protect in [false, true] {
                    let (gm, mut registers) = pae_memory();
                    let entry: Pte = gm.read_plain(read_only_entry).unwrap();
                    gm.write_plain(read_only_entry, &entry.with_read_write(false))
                        .unwrap();
                    registers
                        .ss
                        .attributes
                        .set_descriptor_privilege_level(if user_mode { 3 } else { 0 });
                    if !write_protect {
                        registers.cr0 &= !X64_CR0_WP;
                    }
                    let result = translate_gva_to_gpa(
                        &gm,
                        0x1234,
                        &registers,
                        translation_flags(TranslateMode::Write),
                    );
                    if user_mode || write_protect {
                        assert!(matches!(result, Err(Error::PrivilegeViolation)));
                    } else {
                        assert_eq!(result.unwrap().gpa, 0x4234);
                    }
                }
            }
        }
    }

    #[test]
    fn pae_nx_and_reserved_entries() {
        use crate::emulate::TranslateMode;

        for nx_entry in [0x2000, 0x3008] {
            let (gm, mut registers) = pae_memory();
            let entry: Pte = gm.read_plain(nx_entry).unwrap();
            gm.write_plain(nx_entry, &entry.with_no_execute(true))
                .unwrap();
            for mode in [TranslateMode::Read, TranslateMode::Write] {
                assert_eq!(
                    translate_gva_to_gpa(&gm, 0x1234, &registers, translation_flags(mode))
                        .unwrap()
                        .gpa,
                    0x4234,
                );
            }
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    0x1234,
                    &registers,
                    translation_flags(TranslateMode::Execute)
                ),
                Err(Error::PrivilegeViolation),
            ));
            registers.efer = 0;
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    0x1234,
                    &registers,
                    translation_flags(TranslateMode::Read)
                ),
                Err(Error::InvalidPageTableFlags),
            ));
            gm.write_plain(nx_entry, &entry.with_present(false).with_no_execute(true))
                .unwrap();
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    0x1234,
                    &registers,
                    translation_flags(TranslateMode::Read)
                ),
                Err(Error::PageNotPresent),
            ));
        }
        for reserved_bit in [1, 2, 5, 6, 7, 8, 52, 63] {
            let (gm, registers) = pae_memory();
            gm.write_plain(0x1020, &(0x2001u64 | (1 << reserved_bit)))
                .unwrap();
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    0x1234,
                    &registers,
                    translation_flags(TranslateMode::Read)
                ),
                Err(Error::InvalidPageTableFlags),
            ));
        }
    }

    #[test]
    fn large_pages_validate_reserved_address_bits() {
        for long_mode in [false, true] {
            for pat in [false, true] {
                let (gm, mut registers) = pae_memory();
                let entry = Pte::new()
                    .with_present(true)
                    .with_read_write(true)
                    .with_user(true);
                let leaf_address = if long_mode {
                    registers.efer |= X64_EFER_LMA;
                    registers.cr3 = 0x1000;
                    gm.write_plain(0x1000, &entry.with_pfn(2)).unwrap();
                    0x3000
                } else {
                    0x2000
                };
                let leaf = entry.with_pfn(0x200 + u64::from(pat)).with_pat(true);
                gm.write_plain(leaf_address, &leaf).unwrap();
                let flags = translation_flags(TranslateMode::Read);
                let result = translate_gva_to_gpa(&gm, 0x1234, &registers, flags.clone()).unwrap();
                assert_eq!(result.gpa, 0x20_1234);
                assert!(
                    matches!(result.cache_info, TranslateCachingInfo::Paging { pat_index } if pat_index == u64::from(pat) << 2)
                );
                gm.write_plain(leaf_address, &leaf.with_pfn(0x202)).unwrap();
                assert!(matches!(
                    translate_gva_to_gpa(&gm, 0x1234, &registers, flags),
                    Err(Error::InvalidPageTableFlags),
                ));
            }
        }
    }

    #[test]
    fn pae_supervisor_access_uses_effective_user_permissions() {
        for mode in [
            TranslateMode::Read,
            TranslateMode::Write,
            TranslateMode::Execute,
        ] {
            let (gm, mut registers) = pae_memory();
            registers.ss.attributes.set_descriptor_privilege_level(0);
            registers.efer = 0;
            registers.cr4 |= if mode == TranslateMode::Execute {
                X64_CR4_SMEP
            } else {
                X64_CR4_SMAP
            };
            let flags = translation_flags(mode);
            assert!(matches!(
                translate_gva_to_gpa(&gm, 0x1234, &registers, flags.clone()),
                Err(Error::PrivilegeViolation),
            ));
            let pde: Pte = gm.read_plain(0x2000).unwrap();
            gm.write_plain(0x2000, &pde.with_user(false)).unwrap();
            assert_eq!(
                translate_gva_to_gpa(&gm, 0x1234, &registers, flags)
                    .unwrap()
                    .gpa,
                0x4234
            );
        }
    }

    #[test]
    fn noncanonical_emulated_walk_injects_gp() {
        use crate::emulate::{TranslateGvaSupport, emulate_translate_gva};

        struct Support {
            gm: GuestMemory,
            registers: TranslationRegisters,
        }
        impl TranslateGvaSupport for Support {
            fn guest_memory(&self) -> &GuestMemory {
                &self.gm
            }
            fn acquire_tlb_lock(&mut self) {}
            fn registers(&mut self) -> TranslationRegisters {
                self.registers.clone()
            }
        }
        let (gm, mut registers) = pae_memory();
        registers.efer |= X64_EFER_LMA;
        let mut support = Support { gm, registers };
        let Err(error) =
            emulate_translate_gva(&mut support, 0x0000_8000_0000_0000, TranslateMode::Read)
        else {
            panic!("expected noncanonical address failure");
        };
        assert_eq!(error.code, TranslateGvaResultCode::INTERCEPT);
        let event =
            hvdef::HvX64PendingExceptionEvent::from(u128::from(error.event_info.unwrap().reg_0));
        assert_eq!(
            event.vector(),
            x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into()
        );
        assert!(event.deliver_error_code());
        assert_eq!(event.error_code(), 0);
    }

    #[test]
    fn long_mode_nx_and_page_sizes() {
        use crate::emulate::TranslateMode;

        for la57 in [false, true] {
            let (_, mut registers) = pae_memory();
            let gm = GuestMemory::allocate(0x7000);
            registers.cr3 = 0x1000;
            registers.efer |= X64_EFER_LMA;
            if la57 {
                registers.cr4 |= X64_CR4_LA57;
            }
            let entry = Pte::new()
                .with_present(true)
                .with_read_write(true)
                .with_user(true);
            let gva = if la57 { 0x0001_0000_0000_1234 } else { 0x1234 };
            let root_entry = if la57 { 0x1008 } else { 0x1000 };
            let levels = if la57 { 5 } else { 4 };
            for level in 0..levels {
                let table = (level + 1) * 0x1000;
                let last = level + 1 == levels;
                let address = if level == 0 {
                    root_entry
                } else if last {
                    table + 8
                } else {
                    table
                };
                let pfn = if last { 6 } else { level + 2 };
                gm.write_plain(address, &entry.with_pfn(pfn).with_no_execute(last))
                    .unwrap();
            }
            let read = translation_flags(TranslateMode::Read);
            assert_eq!(
                translate_gva_to_gpa(&gm, gva, &registers, read.clone())
                    .unwrap()
                    .gpa,
                0x6234
            );
            assert!(matches!(
                translate_gva_to_gpa(
                    &gm,
                    gva,
                    &registers,
                    translation_flags(TranslateMode::Execute)
                ),
                Err(Error::PrivilegeViolation),
            ));
            registers.efer &= !X64_EFER_NXE;
            assert!(matches!(
                translate_gva_to_gpa(&gm, gva, &registers, read.clone()),
                Err(Error::InvalidPageTableFlags),
            ));
            registers.efer |= X64_EFER_NXE;
            gm.write_plain(root_entry, &entry.with_pfn(2).with_pat(true))
                .unwrap();
            assert!(matches!(
                translate_gva_to_gpa(&gm, gva, &registers, read),
                Err(Error::InvalidPageTableFlags),
            ));
        }
    }

    #[test]
    fn test_canonical() {
        let cases = &[
            (0, 48, true),
            (0x0000_4000_0000_0000, 48, true),
            (0x0000_8000_0000_0000, 48, false),
            (0x0000_8000_0000_0000, 57, true),
            (0x0100_0000_0000_0000, 57, false),
            (0xffff_ffff_0000_0000, 48, true),
            (0xffff_8000_0000_0000, 48, true),
            (0xffff_0000_0000_0000, 48, false),
            (0xffff_0000_0000_0000, 57, true),
            (0xff00_0000_0000_0000, 57, true),
            (0xfc00_0000_0000_0000, 57, false),
        ];

        for &(addr, bits, is_canonical) in cases {
            assert_eq!(
                is_canonical_address(addr, bits),
                is_canonical,
                "{addr:#x} {bits}"
            );
        }
    }
}
