// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Scalable translation structures (VT-d Rev 4.1, §§3.4.3, 9.2, 9.4-9.6).
//!
//! These are formats, not a scalable-mode lookup implementation. The validators
//! describe the second-stage-only profile: 39/48-bit walks, pass-through, 16-bit
//! domain IDs, CM=0, RID_PASID, second-stage A/D, page-walk snooping and snoop
//! control. First-stage/nested translation, memory types, explicit PASID, ATS,
//! PRI and supervisor requests are unsupported. Defining this profile does not
//! advertise or enable any of its capabilities.
//!
//! Validate only entries used by a request. Not-present entries ignore all
//! fields except FPD (where defined). Callers must retain the original request's
//! IOVA/source ID, accumulate context/directory/table FPD even on not-present
//! entries, and apply it only to *qualified* faults (Table 26, §7.1.3).
//! Format errors alone do not determine fault qualification.

use super::root_context::AddressWidth;
use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Entries in each 4KB lower/upper context table.
pub const SCALABLE_CONTEXT_TABLE_ENTRIES: usize = 128;
/// Entries in each 4KB PASID table.
pub const PASID_TABLE_ENTRIES: u32 = 64;

/// Validation failure in a scalable translation structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScalableValidationError {
    /// All fields except FPD (if any) are ignored.
    #[error("entry not present")]
    NotPresent,
    /// A word has nonzero architecturally reserved bits.
    #[error("reserved bits {bits:#x} in word {word}")]
    ReservedBits {
        /// Zero-based 64-bit word index.
        word: usize,
        /// Offending bits, relative to this word.
        bits: u64,
    },
    /// Optional fields are reserved-zero in the supported profile.
    #[error("unsupported fields {bits:#x} in word {word}")]
    UnsupportedFields {
        /// Zero-based 64-bit word index.
        word: usize,
        /// Offending bits, relative to this word.
        bits: u64,
    },
    /// PGTT is unknown or outside the second-stage-only profile.
    #[error("unsupported PASID translation type {0:?}")]
    UnsupportedTranslationType(PasidTranslationType),
    /// AW does not select a supported second-stage walk.
    #[error("unsupported second-stage address width {0:?}")]
    UnsupportedAddressWidth(AddressWidth),
    /// PASID is outside the range selected by PDTS.
    #[error("PASID {pasid:#x} exceeds directory limit {limit:#x} (exclusive)")]
    PasidOutOfRange {
        /// Requested PASID, without truncation.
        pasid: u32,
        /// Exclusive upper bound on PASID.
        limit: u32,
    },
    /// PDTS must fit in its architectural three-bit field.
    #[error("invalid PASID directory size encoding {0}")]
    InvalidDirectorySize(u8),
    /// A pointer's host address width must be between 12 and 64 bits.
    #[error("invalid host address width {0}")]
    InvalidHostAddressWidth(u8),
}

fn address_reserved_mask(haw: u8) -> Result<u64, ScalableValidationError> {
    match haw {
        12..=63 => Ok(u64::MAX << haw),
        64 => Ok(0),
        _ => Err(ScalableValidationError::InvalidHostAddressWidth(haw)),
    }
}

fn reserved(word: usize, value: u64, mask: u64) -> Result<(), ScalableValidationError> {
    let bits = value & mask;
    if bits != 0 {
        return Err(ScalableValidationError::ReservedBits { word, bits });
    }
    Ok(())
}

fn unsupported(word: usize, value: u64, mask: u64) -> Result<(), ScalableValidationError> {
    let bits = value & mask;
    if bits != 0 {
        return Err(ScalableValidationError::UnsupportedFields { word, bits });
    }
    Ok(())
}

/// Scalable root entry (§9.2), indexed by the bus number.
///
/// Each half independently references a 4KB context table. Guest root entries
/// occupy 16 bytes; their Rust representation has 8-byte alignment.
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ScalableRootEntry {
    /// Devfn 0..=127 (devices 0..=15): LP and LCTP.
    pub lower: ScalableRootEntryHalf,
    /// Devfn 128..=255 (devices 16..=31): UP and UCTP.
    pub upper: ScalableRootEntryHalf,
}

/// One 64-bit half of a scalable root entry (§9.2).
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct ScalableRootEntryHalf {
    /// Lower/upper present bit. Other fields are ignored when clear.
    pub p: bool,
    #[bits(11)]
    _reserved: u64,
    /// Lower/upper context table pointer, bits 63:12 of this half.
    #[bits(52)]
    pub ctp: u64,
}

impl ScalableRootEntry {
    /// Select the half associated with the device/function (§3.4.3).
    pub fn half(&self, devfn: u8) -> ScalableRootEntryHalf {
        if devfn < 128 { self.lower } else { self.upper }
    }
}

impl ScalableRootEntryHalf {
    /// Full, 4KB-aligned context table address.
    pub fn context_table_address(&self) -> u64 {
        self.ctp() << 12
    }

    /// Validate this half; the other half is independent.
    ///
    /// Error word indices are relative to this half, not the full root entry.
    pub fn validate(&self, haw: u8) -> Result<(), ScalableValidationError> {
        if !self.p() {
            return Err(ScalableValidationError::NotPresent);
        }
        reserved(0, self.into_bits(), 0xffe | address_reserved_mask(haw)?)
    }
}

/// Scalable context entry (§9.4), 256 bits.
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ScalableContextEntry {
    /// Present, FPD, optional request controls, PDTS and directory pointer.
    pub lo: ScalableContextEntryLo,
    /// RID_PASID and RID_PRIV.
    pub hi: ScalableContextEntryHi,
    /// Bits 255:128, reserved-zero.
    pub reserved: [u64; 2],
}

/// Scalable context bits 63:0.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct ScalableContextEntryLo {
    /// Present.
    pub p: bool,
    /// Evaluated even when P=0.
    pub fpd: bool,
    /// Device-TLB enable; reserved-zero without ATS.
    pub dte: bool,
    /// Explicit PASID enable; does not govern requests without PASID.
    pub paside: bool,
    /// Page request enable; reserved-zero without PRI.
    pub pre: bool,
    #[bits(4)]
    _reserved: u64,
    /// Directory size: 2^(PDTS+7) entries.
    #[bits(3)]
    pub pdts: u8,
    /// PASID directory address, bits 63:12.
    #[bits(52)]
    pub pasiddirptr: u64,
}

/// Scalable context bits 127:64.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct ScalableContextEntryHi {
    /// Implied PASID for requests without an explicit PASID.
    #[bits(20)]
    pub rid_pasid: u32,
    /// Implied privilege; reserved-zero without RID_PRIV support.
    pub rid_priv: bool,
    #[bits(43)]
    _reserved: u64,
}

/// Decode PDTS without truncating or shifting by an unchecked guest value.
pub fn pasid_directory_entries(pdts: u8) -> Result<u32, ScalableValidationError> {
    match pdts {
        0..=7 => Ok(1u32 << (pdts + 7)),
        _ => Err(ScalableValidationError::InvalidDirectorySize(pdts)),
    }
}

impl ScalableContextEntry {
    /// Index within the selected lower/upper context table (§3.4.3).
    pub fn table_index(devfn: u8) -> usize {
        usize::from(devfn & 0x7f)
    }

    /// Full, 4KB-aligned PASID directory address.
    pub fn directory_address(&self) -> u64 {
        self.lo.pasiddirptr() << 12
    }

    /// Checked directory and table indices for a PASID (§3.4.3).
    ///
    /// This checks both the PDTS bound and the architectural 20-bit limit
    /// before extracting `PASID[19:6]` and `PASID[5:0]`.
    pub fn pasid_indices(&self, pasid: u32) -> Result<(u16, u8), ScalableValidationError> {
        let limit = pasid_directory_entries(self.lo.pdts())? * PASID_TABLE_ENTRIES;
        if pasid >= limit {
            return Err(ScalableValidationError::PasidOutOfRange { pasid, limit });
        }
        Ok(((pasid >> 6) as u16, (pasid & 0x3f) as u8))
    }

    /// Validate for the second-stage-only profile documented by this module.
    pub fn validate(&self, haw: u8) -> Result<(), ScalableValidationError> {
        if !self.lo.p() {
            return Err(ScalableValidationError::NotPresent);
        }
        reserved(0, self.lo.into_bits(), 0x1e0 | address_reserved_mask(haw)?)?;
        reserved(1, self.hi.into_bits(), u64::MAX << 21)?;
        for (index, word) in self.reserved.iter().enumerate() {
            reserved(index + 2, *word, u64::MAX)?;
        }
        unsupported(0, self.lo.into_bits(), 0x1c)?;
        unsupported(1, self.hi.into_bits(), 1 << 20)?;
        self.pasid_indices(self.hi.rid_pasid())?;
        Ok(())
    }
}

/// Scalable PASID-directory entry (§9.5), 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct PasidDirectoryEntry {
    /// Present.
    pub p: bool,
    /// Evaluated even when P=0; OR with the referencing context's FPD.
    pub fpd: bool,
    #[bits(10)]
    _reserved: u64,
    /// Pointer to a 4KB PASID table, bits 63:12.
    #[bits(52)]
    pub smptblptr: u64,
}

impl PasidDirectoryEntry {
    /// Full, 4KB-aligned PASID table address.
    pub fn table_address(&self) -> u64 {
        self.smptblptr() << 12
    }

    /// Validate a directory entry, ignoring other fields when P=0.
    pub fn validate(&self, haw: u8) -> Result<(), ScalableValidationError> {
        if !self.p() {
            return Err(ScalableValidationError::NotPresent);
        }
        reserved(0, self.into_bits(), 0xffc | address_reserved_mask(haw)?)
    }
}

open_enum! {
    /// PASID granular translation type (PGTT, §9.6).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum PasidTranslationType: u8 {
        /// First-stage only (unsupported by the second-stage-only profile).
        FIRST_STAGE = 0b001,
        /// Second-stage only.
        SECOND_STAGE = 0b010,
        /// Nested first/second-stage (unsupported).
        NESTED = 0b011,
        /// Identity mapping.
        PASS_THROUGH = 0b100,
    }
}

/// Scalable PASID-table entry (§9.6), 512 bits.
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct PasidTableEntry {
    /// Present, FPD, AW, PGTT, SSADE and second-stage pointer.
    pub lo: PasidTableEntryLo,
    /// Domain ID and snoop/memory-type controls.
    pub hi: PasidTableEntryHi,
    /// First-stage controls (unsupported except for ignored bits 134:133).
    pub first_stage: PasidTableEntryFirstStage,
    /// Bits 511:192, reserved-zero.
    pub reserved: [u64; 5],
}

/// PASID-table bits 63:0.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct PasidTableEntryLo {
    /// Present.
    pub p: bool,
    /// Evaluated even when P=0; OR with context and directory FPD.
    pub fpd: bool,
    /// Second-stage address width; ignored for pass-through.
    #[bits(3)]
    pub aw: u8,
    #[bits(1)]
    _reserved1: u64,
    /// PASID granular translation type; see [`PasidTranslationType`].
    #[bits(3)]
    pub pgtt: u8,
    /// Enable second-stage accessed/dirty flags; ignored for pass-through.
    pub ssade: bool,
    #[bits(2)]
    _reserved2: u64,
    /// Second-stage root address, bits 63:12; ignored for pass-through
    /// except that bits above HAW remain reserved.
    #[bits(52)]
    pub ssptptr: u64,
}

/// PASID-table bits 127:64.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct PasidTableEntryHi {
    /// Domain identifier (all 16 bits supported, including zero with CM=0).
    #[bits(16)]
    pub did: u16,
    #[bits(7)]
    _reserved1: u64,
    /// Page-walk snoop, full-entry bit 87.
    pub pwsnp: bool,
    /// Page snoop, full-entry bit 88 (supported with ECAP.SC).
    pub pgsnp: bool,
    /// Cache disable; reserved-zero without memory-type support.
    pub cd: bool,
    /// Extended memory type enable; reserved-zero without memory-type support.
    pub emte: bool,
    #[bits(5)]
    _reserved2: u64,
    /// Page attribute table; reserved-zero without memory-type support.
    #[bits(32)]
    pub pat: u32,
}

/// PASID-table bits 191:128.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct PasidTableEntryFirstStage {
    /// Supervisor request enable.
    pub sre: bool,
    #[bits(1)]
    _reserved1: u64,
    /// First-stage paging mode.
    #[bits(2)]
    pub fspm: u8,
    /// Write protect enable.
    pub wpe: bool,
    /// Must be ignored for compatibility with older software (§9.6).
    #[bits(2)]
    _ignored: u64,
    /// Extended accessed flag enable.
    pub eafe: bool,
    #[bits(4)]
    _reserved2: u64,
    /// First-stage root address, bits 191:140 of the full entry.
    #[bits(52)]
    pub fsptptr: u64,
}

impl PasidTableEntry {
    /// Full, 4KB-aligned second-stage root address.
    pub fn page_table_address(&self) -> u64 {
        self.lo.ssptptr() << 12
    }

    /// Validate for the second-stage-only profile documented by this module.
    ///
    /// PWSNP=0 is valid even with SSADE=1. Fault SSS.5 occurs only when an A/D
    /// update is actually needed, not merely when the PASID entry is loaded.
    pub fn validate(&self, haw: u8) -> Result<(), ScalableValidationError> {
        if !self.lo.p() {
            return Err(ScalableValidationError::NotPresent);
        }
        let address_reserved = address_reserved_mask(haw)?;
        reserved(0, self.lo.into_bits(), 0xc20 | address_reserved)?;
        reserved(1, self.hi.into_bits(), 0xf87f_0000)?;
        reserved(2, self.first_stage.into_bits(), 0xf02 | address_reserved)?;
        for (index, word) in self.reserved.iter().enumerate() {
            reserved(index + 3, *word, u64::MAX)?;
        }
        unsupported(1, self.hi.into_bits(), 0xffff_ffff_0600_0000)?;
        unsupported(
            2,
            self.first_stage.into_bits(),
            0x9d | ((u64::MAX << 12) & !address_reserved),
        )?;
        match PasidTranslationType(self.lo.pgtt()) {
            PasidTranslationType::SECOND_STAGE => match AddressWidth(self.lo.aw()) {
                AddressWidth::AW_39BIT | AddressWidth::AW_48BIT => Ok(()),
                aw => Err(ScalableValidationError::UnsupportedAddressWidth(aw)),
            },
            PasidTranslationType::PASS_THROUGH => Ok(()),
            pgtt => Err(ScalableValidationError::UnsupportedTranslationType(pgtt)),
        }
    }
}

const _: () = {
    assert!(size_of::<ScalableRootEntry>() == 16);
    assert!(size_of::<ScalableContextEntry>() == 32);
    assert!(size_of::<PasidDirectoryEntry>() == 8);
    assert!(size_of::<PasidTableEntry>() == 64);
    assert!(align_of::<ScalableRootEntry>() == 8);
    assert!(align_of::<ScalableContextEntry>() == 8);
    assert!(align_of::<PasidDirectoryEntry>() == 8);
    assert!(align_of::<PasidTableEntry>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn decode<T: FromBytes + KnownLayout + Immutable>(words: &[u64]) -> T {
        let bytes: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        T::read_from_bytes(&bytes).unwrap()
    }

    #[test]
    fn test_sizes_alignment_and_offsets() {
        assert_eq!(size_of::<ScalableRootEntry>(), 16);
        assert_eq!(size_of::<ScalableRootEntryHalf>(), 8);
        assert_eq!(size_of::<ScalableContextEntry>(), 32);
        assert_eq!(size_of::<PasidDirectoryEntry>(), 8);
        assert_eq!(size_of::<PasidTableEntry>(), 64);
        assert_eq!(align_of::<ScalableRootEntry>(), 8);
        assert_eq!(align_of::<ScalableContextEntry>(), 8);
        assert_eq!(align_of::<PasidDirectoryEntry>(), 8);
        assert_eq!(align_of::<PasidTableEntry>(), 8);
        assert_eq!(std::mem::offset_of!(ScalableRootEntry, upper), 8);
        assert_eq!(std::mem::offset_of!(ScalableContextEntry, hi), 8);
        assert_eq!(std::mem::offset_of!(ScalableContextEntry, reserved), 16);
        assert_eq!(std::mem::offset_of!(PasidTableEntry, hi), 8);
        assert_eq!(std::mem::offset_of!(PasidTableEntry, first_stage), 16);
        assert_eq!(std::mem::offset_of!(PasidTableEntry, reserved), 24);
        assert_eq!(super::super::root_context::ROOT_TABLE_ENTRIES * 16, 4096);
        assert_eq!(SCALABLE_CONTEXT_TABLE_ENTRIES * 32, 4096);
        assert_eq!(PASID_TABLE_ENTRIES * 64, 4096);
    }

    #[test]
    fn test_root_literal_bytes_and_half_boundaries() {
        let entry: ScalableRootEntry = decode(&[0x1234_5001, 0xabcd_e001]);
        assert!(entry.lower.p());
        assert!(entry.upper.p());
        assert_eq!(entry.lower.context_table_address(), 0x1234_5000);
        assert_eq!(entry.upper.context_table_address(), 0xabcd_e000);
        for devfn in 0..=255 {
            let expected = if devfn < 128 {
                0x1234_5000
            } else {
                0xabcd_e000
            };
            assert_eq!(entry.half(devfn).context_table_address(), expected);
            assert_eq!(
                ScalableContextEntry::table_index(devfn),
                usize::from(devfn % 128)
            );
        }
        for (devfn, offset) in [(0, 0), (127, 4064), (128, 0), (255, 4064)] {
            assert_eq!(ScalableContextEntry::table_index(devfn) * 32, offset);
        }
        let built = ScalableRootEntry {
            lower: ScalableRootEntryHalf::new().with_p(true).with_ctp(0x12345),
            upper: ScalableRootEntryHalf::new().with_p(true).with_ctp(0xabcde),
        };
        assert_eq!(
            built.as_bytes(),
            &[
                1, 0x50, 0x34, 0x12, 0, 0, 0, 0, 1, 0xe0, 0xcd, 0xab, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn test_root_reserved_bits_and_independent_presence() {
        for bit in 1..64 {
            let half = ScalableRootEntryHalf::from(1 | (1 << bit));
            if (1..=11).contains(&bit) || bit >= 48 {
                assert_eq!(
                    half.validate(48),
                    Err(ScalableValidationError::ReservedBits {
                        word: 0,
                        bits: 1 << bit
                    })
                );
            } else {
                assert_eq!(half.validate(48), Ok(()));
            }
        }
        for words in [[1, u64::MAX - 1], [u64::MAX - 1, 1]] {
            let entry: ScalableRootEntry = decode(&words);
            for devfn in [0, 127, 128, 255] {
                let half = entry.half(devfn);
                if half.p() {
                    assert_eq!(half.validate(48), Ok(()));
                } else {
                    assert_eq!(half.validate(48), Err(ScalableValidationError::NotPresent));
                }
            }
        }
    }

    #[test]
    fn test_context_literal_words() {
        let entry: ScalableContextEntry = decode(&[0x1234_5678_9a03, 0x12345, 0, 0]);
        assert!(entry.lo.p());
        assert!(entry.lo.fpd());
        assert_eq!(entry.lo.pdts(), 5);
        assert_eq!(entry.directory_address(), 0x1234_5678_9000);
        assert_eq!(entry.hi.rid_pasid(), 0x12345);
        assert!(!entry.hi.rid_priv());
        assert_eq!(entry.validate(48), Ok(()));
        assert_eq!(
            ScalableContextEntryLo::new()
                .with_p(true)
                .with_fpd(true)
                .with_pdts(5)
                .with_pasiddirptr(0x1_2345_6789)
                .into_bits(),
            0x1234_5678_9a03
        );
        assert_eq!(
            ScalableContextEntryHi::new()
                .with_rid_pasid(0xabcde)
                .with_rid_priv(true)
                .into_bits(),
            0x1abcde
        );
        assert!(ScalableContextEntryLo::from(1 << 2).dte());
        assert!(ScalableContextEntryLo::from(1 << 3).paside());
        assert!(ScalableContextEntryLo::from(1 << 4).pre());
    }

    #[test]
    fn test_context_every_bit_validation() {
        for bit in 0..256 {
            let mut words = [1, 0, 0, 0];
            words[bit / 64] ^= 1 << (bit % 64);
            let entry: ScalableContextEntry = decode(&words);
            let result = entry.validate(48);
            match bit {
                0 => assert_eq!(result, Err(ScalableValidationError::NotPresent)),
                1 | 9..=47 | 64..=76 => assert_eq!(result, Ok(()), "bit {bit}"),
                2..=4 | 84 => assert_eq!(
                    result,
                    Err(ScalableValidationError::UnsupportedFields {
                        word: bit / 64,
                        bits: 1 << (bit % 64),
                    })
                ),
                77..=83 => assert!(matches!(
                    result,
                    Err(ScalableValidationError::PasidOutOfRange { .. })
                )),
                _ => assert_eq!(
                    result,
                    Err(ScalableValidationError::ReservedBits {
                        word: bit / 64,
                        bits: 1 << (bit % 64),
                    })
                ),
            }
        }
    }

    #[test]
    fn test_pdts_and_pasid_boundaries() {
        for pdts in 0..=7 {
            let entries = 128 << pdts;
            let limit = 8192 << pdts;
            assert_eq!(pasid_directory_entries(pdts), Ok(entries));
            let mut context: ScalableContextEntry = decode(&[1 | (u64::from(pdts) << 9), 0, 0, 0]);
            assert_eq!(context.pasid_indices(0), Ok((0, 0)));
            assert_eq!(context.pasid_indices(63), Ok((0, 63)));
            assert_eq!(context.pasid_indices(64), Ok((1, 0)));
            assert_eq!(
                context.pasid_indices(limit - 1),
                Ok(((entries - 1) as u16, 63))
            );
            for pasid in [limit, 1 << 20, u32::MAX] {
                assert_eq!(
                    context.pasid_indices(pasid),
                    Err(ScalableValidationError::PasidOutOfRange { pasid, limit })
                );
            }
            context.hi.set_rid_pasid(limit - 1);
            assert_eq!(context.validate(48), Ok(()));
            if pdts < 7 {
                context.hi.set_rid_pasid(limit);
                assert!(matches!(
                    context.validate(48),
                    Err(ScalableValidationError::PasidOutOfRange { .. })
                ));
            }
        }
        for pdts in 8..=u8::MAX {
            assert_eq!(
                pasid_directory_entries(pdts),
                Err(ScalableValidationError::InvalidDirectorySize(pdts))
            );
        }
    }

    #[test]
    fn test_directory_literal_bytes_and_validation() {
        let entry: PasidDirectoryEntry = decode(&[0x9876_5432_1003]);
        assert!(entry.p());
        assert!(entry.fpd());
        assert_eq!(entry.table_address(), 0x9876_5432_1000);
        assert_eq!(entry.validate(48), Ok(()));
        assert_eq!(
            PasidDirectoryEntry::new()
                .with_p(true)
                .with_fpd(true)
                .with_smptblptr(0x9_8765_4321)
                .as_bytes(),
            &[3, 0x10, 0x32, 0x54, 0x76, 0x98, 0, 0]
        );
        for bit in 1..64 {
            let entry = PasidDirectoryEntry::from(1 | (1 << bit));
            if (2..=11).contains(&bit) || bit >= 48 {
                assert_eq!(
                    entry.validate(48),
                    Err(ScalableValidationError::ReservedBits {
                        word: 0,
                        bits: 1 << bit,
                    })
                );
            } else {
                assert_eq!(entry.validate(48), Ok(()));
            }
        }
    }

    #[test]
    fn test_pasid_table_literal_words_and_bytes() {
        let words = [0x9876_5432_128b, 0x0180_abcd, 0x60, 0, 0, 0, 0, 0];
        let entry: PasidTableEntry = decode(&words);
        assert!(entry.lo.p());
        assert!(entry.lo.fpd());
        assert_eq!(entry.lo.aw(), 2);
        assert_eq!(entry.lo.pgtt(), 2);
        assert!(entry.lo.ssade());
        assert_eq!(entry.page_table_address(), 0x9876_5432_1000);
        assert_eq!(entry.hi.did(), 0xabcd);
        assert!(entry.hi.pwsnp());
        assert!(entry.hi.pgsnp());
        assert_eq!(entry.validate(48), Ok(()));
        let built = PasidTableEntry {
            lo: PasidTableEntryLo::new()
                .with_p(true)
                .with_fpd(true)
                .with_aw(2)
                .with_pgtt(2)
                .with_ssade(true)
                .with_ssptptr(0x9_8765_4321),
            hi: PasidTableEntryHi::new()
                .with_did(0xabcd)
                .with_pwsnp(true)
                .with_pgsnp(true),
            first_stage: PasidTableEntryFirstStage::from(0x60),
            reserved: [0; 5],
        };
        let expected: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        assert_eq!(built.as_bytes(), expected);
        assert_eq!(
            PasidTableEntryLo::new().with_ssade(true).into_bits(),
            1 << 9
        );
        assert_eq!(
            PasidTableEntryHi::new().with_pwsnp(true).into_bits(),
            1 << 23
        );
        assert_eq!(
            PasidTableEntryHi::new().with_pgsnp(true).into_bits(),
            1 << 24
        );
    }

    #[test]
    fn test_pasid_table_reserved_and_unsupported_bits() {
        for bit in 0..512 {
            let word = bit / 64;
            let shift = bit % 64;
            let is_reserved = match word {
                0 => matches!(shift, 5 | 10..=11 | 48..=63),
                1 => matches!(shift, 16..=22 | 27..=31),
                2 => matches!(shift, 1 | 8..=11 | 48..=63),
                _ => true,
            };
            let is_unsupported = match word {
                1 => matches!(shift, 25..=26 | 32..=63),
                2 => matches!(shift, 0 | 2..=4 | 7 | 12..=47),
                _ => false,
            };
            if is_reserved || is_unsupported {
                // Repeat for second-stage and pass-through: unsupported optional
                // fields are reserved-zero even when the translation ignores them.
                for pgtt in [2, 4] {
                    let mut words = [0; 8];
                    words[0] = 1 | (1 << 2) | (pgtt << 6);
                    words[word] |= 1 << shift;
                    let entry: PasidTableEntry = decode(&words);
                    let error = if is_reserved {
                        ScalableValidationError::ReservedBits {
                            word,
                            bits: 1 << shift,
                        }
                    } else {
                        ScalableValidationError::UnsupportedFields {
                            word,
                            bits: 1 << shift,
                        }
                    };
                    assert_eq!(entry.validate(48), Err(error), "bit {bit}, PGTT {pgtt}");
                }
            }
        }
    }

    #[test]
    fn test_pasid_ignored_fields_and_snoop_combinations() {
        for ignored in [0, 1 << 5, 1 << 6, 3 << 5] {
            for pgtt in [2, 4] {
                for ssade in [0, 1] {
                    for pwsnp in [0, 1] {
                        // All 16 DID bits, including DID=0, are legal with CM=0.
                        for did in [0, 0xffff] {
                            let entry: PasidTableEntry = decode(&[
                                1 | (1 << 2) | (pgtt << 6) | (ssade << 9),
                                did | (pwsnp << 23) | (1 << 24),
                                ignored,
                                0,
                                0,
                                0,
                                0,
                                0,
                            ]);
                            assert_eq!(entry.validate(48), Ok(()));
                        }
                    }
                }
            }
        }
        // AW, SSADE and in-range SSPTPTR are ignored for pass-through.
        let passthrough: PasidTableEntry = decode(&[0x0000_ffff_ffff_f31f, 0, 0x60, 0, 0, 0, 0, 0]);
        assert_eq!(passthrough.lo.pgtt(), 4);
        assert_eq!(passthrough.validate(48), Ok(()));
    }

    #[test]
    fn test_pgtt_and_aw_encodings() {
        assert_eq!(PasidTranslationType::FIRST_STAGE.0, 1);
        assert_eq!(PasidTranslationType::SECOND_STAGE.0, 2);
        assert_eq!(PasidTranslationType::NESTED.0, 3);
        assert_eq!(PasidTranslationType::PASS_THROUGH.0, 4);
        for pgtt in 0..=7u8 {
            for aw in 0..=7u8 {
                let entry: PasidTableEntry = decode(&[
                    1 | (u64::from(aw) << 2) | (u64::from(pgtt) << 6),
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ]);
                assert_eq!(PasidTranslationType(entry.lo.pgtt()).0, pgtt);
                let expected = match pgtt {
                    2 if aw == 1 || aw == 2 => Ok(()),
                    2 => Err(ScalableValidationError::UnsupportedAddressWidth(
                        AddressWidth(aw),
                    )),
                    4 => Ok(()),
                    _ => Err(ScalableValidationError::UnsupportedTranslationType(
                        PasidTranslationType(pgtt),
                    )),
                };
                assert_eq!(entry.validate(48), expected);
            }
        }
    }

    #[test]
    fn test_absent_entries_ignore_all_but_fpd() {
        for fpd in [false, true] {
            let lo = !3u64 | ((fpd as u64) << 1);
            let context: ScalableContextEntry = decode(&[lo, u64::MAX, u64::MAX, u64::MAX]);
            let directory = PasidDirectoryEntry::from(lo);
            let mut words = [u64::MAX; 8];
            words[0] = lo;
            let table: PasidTableEntry = decode(&words);
            assert_eq!(
                context.validate(48),
                Err(ScalableValidationError::NotPresent)
            );
            assert_eq!(
                directory.validate(48),
                Err(ScalableValidationError::NotPresent)
            );
            assert_eq!(table.validate(48), Err(ScalableValidationError::NotPresent));
            assert_eq!(context.lo.fpd(), fpd);
            assert_eq!(directory.fpd(), fpd);
            assert_eq!(table.lo.fpd(), fpd);
        }
    }

    #[test]
    fn test_host_address_width_boundaries() {
        for haw in 0..=u8::MAX {
            let root = ScalableRootEntryHalf::from(1);
            let context: ScalableContextEntry = decode(&[1, 0, 0, 0]);
            let directory = PasidDirectoryEntry::from(1);
            let table: PasidTableEntry = decode(&[0x85, 0, 0, 0, 0, 0, 0, 0]);
            let expected = if (12..=64).contains(&haw) {
                Ok(())
            } else {
                Err(ScalableValidationError::InvalidHostAddressWidth(haw))
            };
            assert_eq!(root.validate(haw), expected);
            assert_eq!(context.validate(haw), expected);
            assert_eq!(directory.validate(haw), expected);
            assert_eq!(table.validate(haw), expected);
        }
        for haw in [39, 48, 52, 63] {
            let last_bit = 1u64 << (haw - 1);
            let first_reserved = 1u64 << haw;
            let valid = ScalableRootEntryHalf::from(1 | last_bit);
            assert_eq!(valid.validate(haw), Ok(()));
            let invalid = ScalableRootEntryHalf::from(1 | first_reserved);
            assert_eq!(
                invalid.validate(haw),
                Err(ScalableValidationError::ReservedBits {
                    word: 0,
                    bits: first_reserved,
                })
            );
        }
        assert_eq!(
            ScalableRootEntryHalf::from(1 | (1 << 63)).validate(64),
            Ok(())
        );
    }
}
