// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMU translation logic: STE lookup, CD lookup, and translation context.
//!
//! This module handles the IOVA→GPA translation chain:
//! 1. Look up the Stream Table Entry (STE) by stream ID.
//! 2. Determine the translation action from STE.Config.
//! 3. For S1 translation, look up the Context Descriptor (CD).
//! 4. Extract the translation context (page table base, granule, etc.).

use crate::spec::cd::Cd;
use crate::spec::cd::Tg0;
use crate::spec::events::EventId;
use crate::spec::events::EvtEntry;
use crate::spec::pt::ApBits;
use crate::spec::pt::PtDesc;
use crate::spec::ste::STE_SIZE;
use crate::spec::ste::Ste;
use crate::spec::ste::SteConfig;
use guestmem::GuestMemory;

/// Result of an STE config dispatch.
#[derive(Debug, PartialEq, Eq)]
pub enum SteAction {
    /// Abort all transactions for this stream.
    Abort,
    /// Bypass translation (identity IOVA=GPA).
    Bypass,
    /// Stage 1 translation — proceed to CD lookup.
    S1Translate,
}

/// Parameters for walking an AArch64 stage 1 page table, extracted from
/// STE + CD.
#[derive(Debug, Clone)]
pub struct TranslationContext {
    /// Page table base address (physical/GPA) from CD.TTB0.
    pub ttb0: u64,
    /// Input address size: VA range = 2^(64 - t0sz).
    pub t0sz: u8,
    /// Granule size (4K, 16K, or 64K).
    pub tg0: Tg0,
    /// Output address mask: `(1 << oas_bits) - 1`.
    pub oas_mask: u64,
    /// MAIR0 value (for attribute interpretation — not needed for address
    /// translation yet, but will be used for TLB and memory attribute
    /// emulation).
    pub _mair0: u64,
    /// ASID (for TLB tagging — will be used when a software TLB is added).
    pub _asid: u16,
}

/// Error from STE/CD lookup.
#[derive(Debug)]
pub struct SmmuFault {
    /// The event to write to the EVTQ.
    pub event: EvtEntry,
}

impl SmmuFault {
    fn bad_ste(sid: u32) -> Self {
        SmmuFault {
            event: EvtEntry::bad_ste(sid),
        }
    }

    fn bad_streamid(sid: u32) -> Self {
        SmmuFault {
            event: EvtEntry {
                header: crate::spec::events::EvtHeader::new()
                    .with_event_id(EventId::C_BAD_STREAMID.0),
                sid,
                ..EvtEntry::new()
            },
        }
    }

    fn bad_cd(sid: u32) -> Self {
        SmmuFault {
            event: EvtEntry::bad_cd(sid),
        }
    }
}

/// Look up the STE for a given stream ID.
///
/// `strtab_base` is the physical base address of the linear stream table.
/// `strtab_log2size` is the log2 of the number of entries.
/// `oas_mask` is `(1 << oas_bits) - 1` — the computed STE address is
/// masked to this width per SMMUv3 §3.4 (case 5).
/// Returns the parsed STE or a fault event.
pub fn lookup_ste(
    gm: &GuestMemory,
    strtab_base: u64,
    strtab_log2size: u8,
    sid: u32,
    oas_mask: u64,
) -> Result<Ste, SmmuFault> {
    // Check stream ID is in range. Clamp log2size to 63 to prevent
    // shift overflow on guest-programmed values (the field is 6 bits,
    // so max is 63, but be explicit).
    let max_sid = 1u64 << (strtab_log2size.min(63));
    if (sid as u64) >= max_sid {
        return Err(SmmuFault::bad_streamid(sid));
    }

    let ste_addr = strtab_base.wrapping_add((sid as u64) * (STE_SIZE as u64)) & oas_mask;
    let ste: Ste = gm
        .read_plain(ste_addr)
        .map_err(|_| SmmuFault::bad_ste(sid))?;

    if !ste.valid() {
        return Err(SmmuFault::bad_ste(sid));
    }

    Ok(ste)
}

/// Determine the translation action from an STE's Config field.
pub fn ste_config_action(ste: &Ste) -> Result<SteAction, SteConfig> {
    match ste.config() {
        SteConfig::ABORT => Ok(SteAction::Abort),
        SteConfig::BYPASS => Ok(SteAction::Bypass),
        SteConfig::S1_TRANS => Ok(SteAction::S1Translate),
        other => Err(other),
    }
}

/// Look up the context descriptor for a given STE.
///
/// `ssid` is the sub-stream ID (0 for single-CD setups).
/// `oas_mask` is `(1 << oas_bits) - 1` — the computed CD address is
/// masked to this width per SMMUv3 §3.4 (case 1).
/// Returns the parsed CD or a fault event.
pub fn lookup_cd(
    gm: &GuestMemory,
    ste: &Ste,
    sid: u32,
    ssid: u32,
    oas_mask: u64,
) -> Result<Cd, SmmuFault> {
    // Only linear CD tables are supported. Reject non-linear S1Fmt.
    if ste.s1_fmt() != crate::spec::ste::S1Fmt::LINEAR.0 {
        return Err(SmmuFault::bad_ste(sid));
    }

    let s1_context_ptr = ste.s1_context_ptr();
    let s1_cd_max = ste.s1_cd_max();

    // Validate SSID is within range.
    if s1_cd_max > 0 {
        let max_ssid = 1u32 << s1_cd_max;
        if ssid >= max_ssid {
            return Err(SmmuFault::bad_cd(sid));
        }
    } else if ssid != 0 {
        return Err(SmmuFault::bad_cd(sid));
    }

    let cd_addr =
        s1_context_ptr.wrapping_add((ssid as u64) * (crate::spec::cd::CD_SIZE as u64)) & oas_mask;
    let cd: Cd = gm.read_plain(cd_addr).map_err(|_| SmmuFault::bad_cd(sid))?;

    if !cd.valid() {
        return Err(SmmuFault::bad_cd(sid));
    }

    // Only AArch64 page tables are supported.
    if !cd.aa64() {
        return Err(SmmuFault::bad_cd(sid));
    }

    Ok(cd)
}

/// Extract the translation context from a parsed CD.
///
/// `device_oas_mask` is `(1 << device_oas_bits) - 1` from the SMMU's
/// IDR5.OAS. Per SMMUv3 §3.4, CD.IPS is capped to the device OAS.
///
/// Returns `Err` with a `SmmuFault` if the CD contains unsupported or
/// invalid configuration (e.g., unrecognized granule or IPS encoding).
pub fn translation_context(
    cd: &Cd,
    sid: u32,
    device_oas_mask: u64,
) -> Result<TranslationContext, SmmuFault> {
    let tg0 = cd.tg0();
    let ips = cd.ips();

    // Validate granule.
    if tg0.granule_size().is_none() {
        return Err(SmmuFault::bad_cd(sid));
    }

    // Validate IPS and cap to device OAS per SMMUv3 §3.4.
    let cd_oas_bits = ips.bits().ok_or_else(|| SmmuFault::bad_cd(sid))?;
    let cd_oas_mask = (1u64 << cd_oas_bits) - 1;
    // The effective OAS is the minimum of CD.IPS and the device OAS.
    let oas_mask = cd_oas_mask.min(device_oas_mask);

    let t0sz = cd.t0sz();
    if t0sz > 48 {
        return Err(SmmuFault::bad_cd(sid));
    }

    // EPD0=1 means TTB0 walks are disabled — all accesses fault.
    if cd.epd0() {
        return Err(SmmuFault::bad_cd(sid));
    }

    Ok(TranslationContext {
        ttb0: cd.ttb0(),
        t0sz,
        tg0,
        oas_mask,
        _mair0: cd.mair0,
        _asid: cd.asid(),
    })
}

/// Result of a successful page table walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    /// Translated guest physical address (with page offset applied).
    pub gpa: u64,
    /// Page size of the mapping (granule for pages, block size for blocks).
    pub page_size: u64,
}

/// Compute the start level and number of VA bits for a given granule and T0SZ.
///
/// Returns `(start_level, va_bits)` where `va_bits = 64 - t0sz`.
fn compute_start_level(tg0: Tg0, t0sz: u8) -> Option<(u8, u8)> {
    let va_bits = 64u8.checked_sub(t0sz)?;
    let bits_per_level = tg0.bits_per_level()?;
    let page_shift = tg0.page_shift()?;

    // Number of address bits resolved by the page table walk (excluding page
    // offset). For 4K/9 bits per level: va_bits - 12 bits are resolved by
    // the walk.
    let resolve_bits = va_bits.checked_sub(page_shift)?;

    // Number of full levels needed = ceil(resolve_bits / bits_per_level).
    // Start level = 4 - num_levels (levels are numbered 0..3).
    // num_levels == 0 means the VA space is exactly one page (no walk
    // needed), which is an invalid configuration.
    let num_levels = resolve_bits.div_ceil(bits_per_level);
    if num_levels == 0 || num_levels > 4 {
        return None;
    }
    let start_level = 4 - num_levels;

    Some((start_level, va_bits))
}

/// Walk AArch64 stage 1 translation tables to translate an IOVA to a GPA.
///
/// `gm` is the guest memory (for reading page table entries from guest RAM).
/// `ctx` holds the page table root and configuration (from STE+CD).
/// `iova` is the input virtual address to translate.
/// `write` is true for write accesses (for permission checking).
/// `sid` is the stream ID (for fault event construction).
///
/// Returns the translated GPA and page size, or an `SmmuFault` with the
/// event to report.
pub fn walk_s1(
    gm: &GuestMemory,
    ctx: &TranslationContext,
    iova: u64,
    write: bool,
    sid: u32,
) -> Result<Translation, SmmuFault> {
    let tg0 = ctx.tg0;
    let page_shift = tg0.page_shift().ok_or_else(|| SmmuFault {
        event: EvtEntry::translation_fault(sid, iova, write),
    })?;
    let bits_per_level = tg0.bits_per_level().ok_or_else(|| SmmuFault {
        event: EvtEntry::translation_fault(sid, iova, write),
    })?;

    let (start_level, va_bits) = compute_start_level(tg0, ctx.t0sz).ok_or_else(|| SmmuFault {
        event: EvtEntry::translation_fault(sid, iova, write),
    })?;

    // Check IOVA is within the valid range (2^va_bits).
    let va_mask = if va_bits >= 64 {
        u64::MAX
    } else {
        (1u64 << va_bits) - 1
    };
    if iova > va_mask {
        return Err(SmmuFault {
            event: EvtEntry::translation_fault(sid, iova, write),
        });
    }

    let oas_mask = ctx.oas_mask;

    let mut table_addr = ctx.ttb0;
    let mut level = start_level;

    loop {
        // Compute the index at this level.
        // For level `l` with 4K granule (9 bits/level, 12-bit page offset):
        //   Level 0: bits [47:39] (9 bits)
        //   Level 1: bits [38:30] (9 bits)
        //   Level 2: bits [29:21] (9 bits)
        //   Level 3: bits [20:12] (9 bits)
        // General formula: shift = page_shift + (3 - level) * bits_per_level
        let shift = page_shift as u32 + (3 - level as u32) * bits_per_level as u32;
        let index_mask = (1u64 << bits_per_level) - 1;

        // For the start level, the number of index bits may be smaller than
        // bits_per_level when va_bits is not a multiple of bits_per_level.
        let index = (iova >> shift) & index_mask;

        // Truncate the descriptor address to the OAS. Per SMMUv3 §3.4,
        // translation table walk addresses are PA values subject to OAS
        // truncation.
        let desc_addr = table_addr.wrapping_add(index * 8) & oas_mask;
        let desc: PtDesc = gm.read_plain(desc_addr).map_err(|_| SmmuFault {
            event: EvtEntry::translation_fault(sid, iova, write),
        })?;

        if !desc.is_valid() {
            return Err(SmmuFault {
                event: EvtEntry::translation_fault(sid, iova, write),
            });
        }

        if level == 3 {
            // At level 3, type=1 means page, type=0 is reserved (fault).
            if !desc.desc_type() {
                return Err(SmmuFault {
                    event: EvtEntry::translation_fault(sid, iova, write),
                });
            }
        }

        if level == 3 || desc.is_block() {
            // Leaf descriptor (page at L3 or block at L1/L2).
            check_permissions(&desc, iova, write, sid)?;
            // The output address is the descriptor's address field masked
            // to the mapping size. This naturally handles all granule sizes
            // because `shift` encodes the correct alignment for any
            // granule/level combination.
            let mapping_size = 1u64 << shift;
            let output_addr = (desc.addr_bits() << 12) & !(mapping_size - 1);
            if output_addr > oas_mask {
                return Err(SmmuFault {
                    event: EvtEntry::addr_size_fault(sid, iova, write),
                });
            }
            let offset = iova & (mapping_size - 1);
            return Ok(Translation {
                gpa: output_addr | offset,
                page_size: mapping_size,
            });
        }

        // Table descriptor — descend to next level.
        // Mask RES0 bits below granule alignment and check against OAS.
        // Per SMMUv3 §3.4, addresses derived from intermediate translation
        // table descriptors generate an Address Size fault if they exceed
        // the OAS.
        let next_addr = desc.next_table_addr(page_shift);
        if next_addr > oas_mask {
            return Err(SmmuFault {
                event: EvtEntry::addr_size_fault(sid, iova, write),
            });
        }
        table_addr = next_addr;
        level += 1;

        if level > 3 {
            // Should not happen with well-formed page tables.
            return Err(SmmuFault {
                event: EvtEntry::translation_fault(sid, iova, write),
            });
        }
    }
}

/// Check access permissions and access flag on a leaf descriptor.
fn check_permissions(desc: &PtDesc, iova: u64, write: bool, sid: u32) -> Result<(), SmmuFault> {
    // Check access flag.
    if !desc.af() {
        return Err(SmmuFault {
            event: EvtEntry::access_fault(sid, iova, write),
        });
    }

    // Check write permission.
    if write {
        let ap = ApBits(desc.ap());
        if !ap.allows_write() {
            return Err(SmmuFault {
                event: EvtEntry::permission_fault(sid, iova, write),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::cd::CD_SIZE;
    use crate::spec::cd::CdDw0;
    use crate::spec::cd::CdDw1;
    use crate::spec::cd::Ips;
    use crate::spec::ste::SteDw0;
    use crate::spec::ste::SteDw1;

    const STRTAB_BASE: u64 = 0x10_0000;
    const CD_BASE: u64 = 0x20_0000;
    const STRTAB_LOG2SIZE: u8 = 10; // 1024 entries
    const OAS_MASK: u64 = (1 << 40) - 1; // 40-bit OAS

    /// Build a valid STE for S1 translation pointing to a CD table.
    fn make_s1_ste(cd_base: u64) -> Ste {
        Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_context_ptr(cd_base >> 6)
                .with_s1_cd_max(0), // single CD
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    /// Build a valid STE for bypass.
    fn make_bypass_ste() -> Ste {
        Ste {
            qw0: SteDw0::new().with_v(true).with_config(SteConfig::BYPASS.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    /// Build a valid STE for abort.
    fn make_abort_ste() -> Ste {
        Ste {
            qw0: SteDw0::new().with_v(true).with_config(SteConfig::ABORT.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    /// Build a valid CD.
    fn make_cd(ttb0: u64, t0sz: u8, tg0: Tg0, ips: Ips) -> Cd {
        Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(t0sz)
                .with_tg0(tg0.0)
                .with_ips(ips.0)
                .with_aa64(true)
                .with_asid(1),
            qw1: CdDw1::new().with_ttb0(ttb0 >> 4),
            _qw2: 0,
            mair0: 0xFF440C0400,
            mair1: 0,
            _qw5_7: [0; 3],
        }
    }

    /// Write an STE to guest memory at the given stream ID.
    fn write_ste(gm: &GuestMemory, sid: u32, ste: &Ste) {
        let addr = STRTAB_BASE + (sid as u64) * (STE_SIZE as u64);
        gm.write_plain(addr, ste).expect("write STE");
    }

    /// Write a CD to guest memory at the given SSID offset from cd_base.
    fn write_cd(gm: &GuestMemory, cd_base: u64, ssid: u32, cd: &Cd) {
        let addr = cd_base + (ssid as u64) * (CD_SIZE as u64);
        gm.write_plain(addr, cd).expect("write CD");
    }

    // =========================================================================
    // STE lookup tests
    // =========================================================================

    #[test]
    fn test_ste_lookup_valid() {
        let gm = GuestMemory::allocate(0x40_0000);
        let ste = make_s1_ste(CD_BASE);
        write_ste(&gm, 5, &ste);

        let result = lookup_ste(&gm, STRTAB_BASE, STRTAB_LOG2SIZE, 5, OAS_MASK);
        let found = result.expect("STE lookup should succeed");
        assert!(found.valid());
        assert_eq!(found.config(), SteConfig::S1_TRANS);
        assert_eq!(found.s1_context_ptr(), CD_BASE);
    }

    #[test]
    fn test_ste_lookup_invalid_v0() {
        let gm = GuestMemory::allocate(0x40_0000);
        // Write an STE with V=0.
        let ste = Ste {
            qw0: SteDw0::new().with_v(false),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        write_ste(&gm, 3, &ste);

        let result = lookup_ste(&gm, STRTAB_BASE, STRTAB_LOG2SIZE, 3, OAS_MASK);
        let fault = result.expect_err("Should fault on V=0");
        assert_eq!(fault.event.event_id(), EventId::C_BAD_STE);
        assert_eq!(fault.event.sid, 3);
    }

    #[test]
    fn test_ste_lookup_out_of_range() {
        let gm = GuestMemory::allocate(0x40_0000);
        // Stream ID 2048 is out of range for log2size=10 (max 1024).
        let result = lookup_ste(&gm, STRTAB_BASE, STRTAB_LOG2SIZE, 2048, OAS_MASK);
        let fault = result.expect_err("Should fault on out-of-range SID");
        assert_eq!(fault.event.event_id(), EventId::C_BAD_STREAMID);
    }

    // =========================================================================
    // STE config dispatch tests
    // =========================================================================

    #[test]
    fn test_ste_config_abort() {
        let ste = make_abort_ste();
        assert_eq!(ste_config_action(&ste), Ok(SteAction::Abort));
    }

    #[test]
    fn test_ste_config_bypass() {
        let ste = make_bypass_ste();
        assert_eq!(ste_config_action(&ste), Ok(SteAction::Bypass));
    }

    #[test]
    fn test_ste_config_s1_trans() {
        let ste = make_s1_ste(CD_BASE);
        assert_eq!(ste_config_action(&ste), Ok(SteAction::S1Translate));
    }

    #[test]
    fn test_ste_config_unknown() {
        // Config = 0b010 is not a valid configuration.
        let ste = Ste {
            qw0: SteDw0::new().with_v(true).with_config(0b010),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        assert!(ste_config_action(&ste).is_err());
    }

    // =========================================================================
    // CD lookup tests
    // =========================================================================

    #[test]
    fn test_cd_lookup_valid() {
        let gm = GuestMemory::allocate(0x40_0000);
        let ste = make_s1_ste(CD_BASE);
        let cd = make_cd(0x3000_0000, 32, Tg0::GRAN_4K, Ips::IPS_40);
        write_cd(&gm, CD_BASE, 0, &cd);

        let result = lookup_cd(&gm, &ste, 5, 0, OAS_MASK);
        let found = result.expect("CD lookup should succeed");
        assert!(found.valid());
        assert!(found.aa64());
        assert_eq!(found.ttb0(), 0x3000_0000);
        assert_eq!(found.t0sz(), 32);
    }

    #[test]
    fn test_cd_lookup_invalid_v0() {
        let gm = GuestMemory::allocate(0x40_0000);
        let ste = make_s1_ste(CD_BASE);
        // Write a CD with V=0.
        let cd = Cd {
            qw0: CdDw0::new().with_v(false),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        write_cd(&gm, CD_BASE, 0, &cd);

        let result = lookup_cd(&gm, &ste, 5, 0, OAS_MASK);
        let fault = result.expect_err("Should fault on V=0 CD");
        assert_eq!(fault.event.event_id(), EventId::C_BAD_CD);
    }

    #[test]
    fn test_cd_lookup_not_aa64() {
        let gm = GuestMemory::allocate(0x40_0000);
        let ste = make_s1_ste(CD_BASE);
        // Write a CD with AA64=0 (AArch32 — not supported).
        let cd = Cd {
            qw0: CdDw0::new().with_v(true).with_aa64(false),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        write_cd(&gm, CD_BASE, 0, &cd);

        let result = lookup_cd(&gm, &ste, 5, 0, OAS_MASK);
        let fault = result.expect_err("Should fault on non-AA64 CD");
        assert_eq!(fault.event.event_id(), EventId::C_BAD_CD);
    }

    // =========================================================================
    // Translation context tests
    // =========================================================================

    #[test]
    fn test_translation_context_4k() {
        let cd = make_cd(0x4000_0000, 32, Tg0::GRAN_4K, Ips::IPS_40);
        let ctx = translation_context(&cd, 0, OAS_MASK).expect("should succeed");
        assert_eq!(ctx.ttb0, 0x4000_0000);
        assert_eq!(ctx.t0sz, 32);
        assert_eq!(ctx.tg0, Tg0::GRAN_4K);
        assert_eq!(ctx.oas_mask, OAS_MASK);
        assert_eq!(ctx._asid, 1);
    }

    #[test]
    fn test_translation_context_16k() {
        let cd = make_cd(0x8000_0000, 28, Tg0::GRAN_16K, Ips::IPS_48);
        let ctx = translation_context(&cd, 0, (1u64 << 48) - 1).expect("should succeed");
        assert_eq!(ctx.tg0, Tg0::GRAN_16K);
        assert_eq!(ctx.oas_mask, (1 << 48) - 1);
        assert_eq!(ctx.t0sz, 28);
    }

    #[test]
    fn test_translation_context_bad_granule() {
        // TG0 = 0b11 is reserved/invalid.
        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(0b11) // invalid
                .with_ips(Ips::IPS_40.0)
                .with_aa64(true),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        let result = translation_context(&cd, 0, OAS_MASK);
        assert!(result.is_err());
    }

    #[test]
    fn test_translation_context_bad_ips() {
        // IPS = 0b111 is reserved/invalid.
        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(0b111) // invalid
                .with_aa64(true),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        let result = translation_context(&cd, 0, OAS_MASK);
        assert!(result.is_err());
    }

    #[test]
    fn test_translation_context_epd0() {
        // EPD0=1 disables TTB0 walks.
        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(Ips::IPS_40.0)
                .with_aa64(true)
                .with_epd0(true),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        let result = translation_context(&cd, 0, OAS_MASK);
        assert!(result.is_err());
    }

    // =========================================================================
    // Page table walker tests
    // =========================================================================

    // Page table memory layout constants.
    const PT_L0_BASE: u64 = 0x30_0000; // L0 table
    const PT_L1_BASE: u64 = 0x30_1000; // L1 table
    const PT_L2_BASE: u64 = 0x30_2000; // L2 table
    const PT_L3_BASE: u64 = 0x30_3000; // L3 table
    const DATA_GPA: u64 = 0x4000_0000; // Target GPA for mappings

    /// Build a TranslationContext for 4K granule, T0SZ=32 (32-bit VA), 40-bit OAS.
    fn make_4k_ctx(ttb0: u64) -> TranslationContext {
        TranslationContext {
            ttb0,
            t0sz: 32,
            tg0: Tg0::GRAN_4K,
            oas_mask: OAS_MASK,
            _mair0: 0xFF440C0400,
            _asid: 1,
        }
    }

    /// Write a page table descriptor at the given address.
    fn write_pt_desc(gm: &GuestMemory, addr: u64, desc: u64) {
        gm.write_plain(addr, &desc).expect("write PT desc");
    }

    /// Build a table descriptor pointing to the given next-level table address.
    fn table_desc(next_table: u64) -> u64 {
        // Valid=1, Type=1 (table), address in bits [47:12].
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true) // table
            .with_addr_bits(next_table >> 12);
        desc.into()
    }

    /// Build a block descriptor for a given output address with RW, AF set.
    fn block_desc(output_addr: u64) -> u64 {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(false) // block
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(output_addr >> 12);
        desc.into()
    }

    /// Build a page descriptor (L3) for a given output address with RW, AF set.
    fn page_desc(output_addr: u64) -> u64 {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true) // page at L3
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(output_addr >> 12);
        desc.into()
    }

    /// Build a read-only page descriptor.
    fn ro_page_desc(output_addr: u64) -> u64 {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(true)
            .with_ap(ApBits::RO_EL1.0)
            .with_addr_bits(output_addr >> 12);
        desc.into()
    }

    /// Build a page descriptor with AF=0 (access flag not set).
    fn no_af_page_desc(output_addr: u64) -> u64 {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(false)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(output_addr >> 12);
        desc.into()
    }

    #[test]
    fn test_walk_4k_single_level_block() {
        // T0SZ=32 with 4K granule: 32-bit VA space.
        // Walk starts at level 1 (levels 1, 2, 3).
        // Map a 1GB block at level 1 entry 0 → DATA_GPA.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // Level 1 entry 0: 1GB block → DATA_GPA.
        write_pt_desc(&gm, PT_L1_BASE, block_desc(DATA_GPA));

        let result = walk_s1(&gm, &ctx, 0, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA);
        assert_eq!(tr.page_size, 1 << 30); // 1GB block
    }

    #[test]
    fn test_walk_4k_four_levels() {
        // T0SZ=16 with 4K granule: 48-bit VA space, 4 levels (0-3).
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = TranslationContext {
            ttb0: PT_L0_BASE,
            t0sz: 16,
            tg0: Tg0::GRAN_4K,
            oas_mask: (1 << 48) - 1,
            _mair0: 0,
            _asid: 0,
        };

        // L0[0] → L1 table
        write_pt_desc(&gm, PT_L0_BASE, table_desc(PT_L1_BASE));
        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → page at DATA_GPA
        write_pt_desc(&gm, PT_L3_BASE, page_desc(DATA_GPA));

        let result = walk_s1(&gm, &ctx, 0, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA);
        assert_eq!(tr.page_size, 4096);
    }

    #[test]
    fn test_walk_4k_2mb_block() {
        // T0SZ=32, 4K granule. Level 2 block descriptor (2MB).
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → 2MB block at DATA_GPA
        write_pt_desc(&gm, PT_L2_BASE, block_desc(DATA_GPA));

        let result = walk_s1(&gm, &ctx, 0, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA);
        assert_eq!(tr.page_size, 2 << 20); // 2MB
    }

    #[test]
    fn test_walk_4k_page_with_offset() {
        // Walk to a 4K page and verify the intra-page offset is preserved.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → page at DATA_GPA
        write_pt_desc(&gm, PT_L3_BASE, page_desc(DATA_GPA));

        // Access IOVA 0x0000_0100 — should map to DATA_GPA + 0x100.
        let result = walk_s1(&gm, &ctx, 0x100, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA + 0x100);
        assert_eq!(tr.page_size, 4096);
    }

    #[test]
    fn test_walk_4k_block_with_offset() {
        // Walk to a 2MB block and verify the intra-block offset is preserved.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → 2MB block at DATA_GPA
        write_pt_desc(&gm, PT_L2_BASE, block_desc(DATA_GPA));

        // Access IOVA 0x0001_2345 — should map to DATA_GPA + 0x0001_2345.
        let result = walk_s1(&gm, &ctx, 0x0001_2345, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA + 0x0001_2345);
        assert_eq!(tr.page_size, 2 << 20);
    }

    #[test]
    fn test_walk_fault_unmapped() {
        // Walk with a PTE that has Valid=0.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] is all zeros (invalid).
        let result = walk_s1(&gm, &ctx, 0, false, 42);
        let fault = result.expect_err("should fault");
        assert_eq!(fault.event.event_id(), EventId::F_TRANSLATION);
        assert_eq!(fault.event.sid, 42);
    }

    #[test]
    fn test_walk_fault_permission() {
        // Write to a read-only page.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → read-only page
        write_pt_desc(&gm, PT_L3_BASE, ro_page_desc(DATA_GPA));

        // Read should succeed.
        let result = walk_s1(&gm, &ctx, 0, false, 0);
        assert!(result.is_ok());

        // Write should fault.
        let result = walk_s1(&gm, &ctx, 0, true, 0);
        let fault = result.expect_err("should fault on write to RO");
        assert_eq!(fault.event.event_id(), EventId::F_PERMISSION);
    }

    #[test]
    fn test_walk_fault_access_flag() {
        // Page with AF=0 — should produce F_ACCESS fault.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → page with AF=0
        write_pt_desc(&gm, PT_L3_BASE, no_af_page_desc(DATA_GPA));

        let result = walk_s1(&gm, &ctx, 0, false, 0);
        let fault = result.expect_err("should fault on AF=0");
        assert_eq!(fault.event.event_id(), EventId::F_ACCESS);
    }

    #[test]
    fn test_walk_fault_addr_size() {
        // Output address exceeds OAS.
        let gm = GuestMemory::allocate(0x5000_0000);
        // 32-bit OAS — output addresses must fit in 32 bits.
        let ctx = TranslationContext {
            ttb0: PT_L1_BASE,
            t0sz: 32,
            tg0: Tg0::GRAN_4K,
            oas_mask: (1 << 32) - 1,
            _mair0: 0,
            _asid: 0,
        };

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → page at a high address (exceeds 32-bit OAS)
        let high_addr = 0x2_0000_0000u64; // 8GB, exceeds 32-bit
        write_pt_desc(&gm, PT_L3_BASE, page_desc(high_addr));

        let result = walk_s1(&gm, &ctx, 0, false, 0);
        let fault = result.expect_err("should fault on addr size");
        assert_eq!(fault.event.event_id(), EventId::F_ADDR_SIZE);
    }

    #[test]
    fn test_walk_iova_out_of_range() {
        // IOVA exceeds the VA range defined by T0SZ.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE); // T0SZ=32, VA range = 2^32

        // IOVA = 0x1_0000_0000 (exceeds 32-bit range).
        let result = walk_s1(&gm, &ctx, 0x1_0000_0000, false, 0);
        let fault = result.expect_err("should fault on out-of-range IOVA");
        assert_eq!(fault.event.event_id(), EventId::F_TRANSLATION);
    }

    #[test]
    fn test_walk_nonzero_l1_index() {
        // Verify that non-zero L1 indices work correctly.
        // T0SZ=32, 4K: L1 has 4 entries (indices 0-3, each covering 1GB).
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[2] → 1GB block at DATA_GPA (IOVA starting at 2GB).
        let l1_entry2_addr = PT_L1_BASE + 2 * 8;
        write_pt_desc(&gm, l1_entry2_addr, block_desc(DATA_GPA));

        // IOVA = 0x8000_0000 (2GB) should use L1 index 2.
        let result = walk_s1(&gm, &ctx, 0x8000_0000, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, DATA_GPA);
        assert_eq!(tr.page_size, 1 << 30);
    }

    #[test]
    fn test_walk_nonzero_l3_index() {
        // Verify non-zero L3 index with 4K pages.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        // L1[0] → L2 table
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3 table
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[5] → page at DATA_GPA + 0x5000
        let target = DATA_GPA + 0x5000;
        write_pt_desc(&gm, PT_L3_BASE + 5 * 8, page_desc(target));

        // IOVA = 0x5000 (L3 index 5) + offset 0x42.
        let result = walk_s1(&gm, &ctx, 0x5042, false, 0);
        let tr = result.expect("should translate");
        assert_eq!(tr.gpa, target + 0x42);
        assert_eq!(tr.page_size, 4096);
    }

    #[test]
    fn test_walk_write_to_rw_page() {
        // Write to a RW page should succeed.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = make_4k_ctx(PT_L1_BASE);

        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        write_pt_desc(&gm, PT_L3_BASE, page_desc(DATA_GPA));

        let result = walk_s1(&gm, &ctx, 0, true, 0);
        let tr = result.expect("write to RW page should succeed");
        assert_eq!(tr.gpa, DATA_GPA);
    }

    #[test]
    fn test_compute_start_level_4k() {
        // T0SZ=32, 4K: VA bits=32, resolve=20, levels=ceil(20/9)=3, start=1
        assert_eq!(compute_start_level(Tg0::GRAN_4K, 32), Some((1, 32)));
        // T0SZ=16, 4K: VA bits=48, resolve=36, levels=4, start=0
        assert_eq!(compute_start_level(Tg0::GRAN_4K, 16), Some((0, 48)));
        // T0SZ=25, 4K: VA bits=39, resolve=27, levels=3, start=1
        assert_eq!(compute_start_level(Tg0::GRAN_4K, 25), Some((1, 39)));
    }

    #[test]
    fn test_walk_degenerate_t0sz_returns_fault() {
        // 64KB granule with T0SZ=48 produces resolve_bits=0. Without
        // the guard in compute_start_level, walk_s1 would compute
        // start_level=4 and then evaluate (3u32 - 4u32), panicking
        // in debug mode. Verify it returns a translation fault instead.
        let gm = GuestMemory::allocate(0x5000_0000);
        let ctx = TranslationContext {
            ttb0: PT_L1_BASE,
            t0sz: 48,
            tg0: Tg0::GRAN_64K,
            oas_mask: OAS_MASK,
            _mair0: 0,
            _asid: 0,
        };

        let result = walk_s1(&gm, &ctx, 0, false, 99);
        let fault = result.expect_err("degenerate T0SZ must fault, not panic");
        assert_eq!(fault.event.event_id(), EventId::F_TRANSLATION);
        assert_eq!(fault.event.sid, 99);
    }
}
