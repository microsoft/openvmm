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
use x86defs::X64_CR4_LA57;
use x86defs::X64_CR4_PAE;
use x86defs::X64_CR4_PSE;
use x86defs::X64_CR4_SMAP;
use x86defs::X64_CR4_SMEP;
use x86defs::X64_EFER_LMA;
use x86defs::X64_EFER_NXE;

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
        /// Page-level cache disable bit in PTE
        cache_disable: bool,
        /// Page-level write-through bit in PTE
        write_through: bool,
        /// PAT bit in PTE
        pat_supported: bool,
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
    mut flags: TranslateFlags,
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

    // Determine the permission requirements of the walk according to the
    // current mode.
    if registers.efer & X64_EFER_NXE == 0 {
        flags.validate_execute = false;
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

    let mut no_user_access = supervisor_access
        && ((flags.validate_execute && registers.cr4 & X64_CR4_SMEP != 0)
            || ((flags.validate_read || flags.validate_write)
                && !flags.override_smap
                && registers.cr4 & X64_CR4_SMAP != 0
                && (flags.enforce_smap || !RFlags::from(registers.rflags).alignment_check())));

    let mut gpa_base = registers.cr3 & !0xfff;
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

            if registers.efer & X64_EFER_LMA == 0 {
                if pte.available1() != 0 || (registers.efer & X64_EFER_NXE != 0 && pte.no_execute())
                {
                    return Err(Error::InvalidPageTableFlags);
                }
            }

            if !pte.present() {
                tracing::trace!(pte_address, ?pte, "page not present");
                return Err(Error::PageNotPresent);
            }

            if (flags.validate_write && !pte.read_write())
                || (flags.validate_execute && pte.no_execute())
                || (user_access && !pte.user())
            {
                return Err(Error::PrivilegeViolation);
            }

            // Determine whether this is the terminal PTE.
            let done = remaining_bits == 12
                || (registers.cr4 & (X64_CR4_PAE | X64_CR4_PSE) != 0 && pte.pat());

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
            if flags.set_page_table_bits {
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
        if !pte.user() {
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
    Ok(TranslateResult {
        gpa: (gpa_base & address_mask) | (gva & !address_mask),
        cache_info: TranslateCachingInfo::Paging {
            cache_disable,
            write_through,
            pat_supported,
        },
    })
}

/// Returns whether a virtual address is canonical. On x86-64, this means that
/// the N top unused bits are equal to the top used bit, where N is 64 minus the
/// number of effective address bits (48 or 57).
fn is_canonical_address(gva: u64, address_bits: u32) -> bool {
    // Shift out the address bits that aren't part of the check, sign extending.
    // This makes the subsequent check an easy comparison.
    let high_bits = (gva as i64) >> (address_bits - 1);
    high_bits == 0 || high_bits == -1
}

#[cfg(test)]
mod tests {
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
                super::is_canonical_address(addr, bits),
                is_canonical,
                "{addr:#x} {bits}"
            );
        }
    }
}
