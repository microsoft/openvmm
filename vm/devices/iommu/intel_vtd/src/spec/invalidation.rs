// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Invalidation queue descriptor types for the Intel VT-d IOMMU.
//!
//! Based on Intel VT-d Specification Rev 4.1, §§6.5.2 and 11.4.9.
//! Queue entries are 128 or 256 bits. Type is the concatenation of bits
//! 11:9 and 3:0, not just the low nibble. Legacy descriptors in a 256-bit
//! queue have zero upper padding.

use super::registers::CapReg;
use super::registers::EcapReg;
use super::registers::TranslationTableMode;
use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A raw 128-bit invalidation queue descriptor (16 bytes).
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct InvalidationDescriptor {
    /// First dword: bits 11:9 and 3:0 = type, rest is type-dependent.
    pub dw0: u32,
    /// Second dword: type-dependent fields.
    pub dw1: u32,
    /// Third dword: type-dependent fields.
    pub dw2: u32,
    /// Fourth dword: type-dependent fields.
    pub dw3: u32,
}

impl InvalidationDescriptor {
    /// Extract `Type[6:0]` from the two noncontiguous fields.
    pub fn descriptor_type(&self) -> DescriptorType {
        DescriptorType(((self.dw0 & 0xf) | ((self.dw0 >> 5) & 0x70)) as u8)
    }

    /// Bits 63:0 of the descriptor.
    pub fn low(&self) -> u64 {
        u64::from(self.dw0) | (u64::from(self.dw1) << 32)
    }

    /// Bits 127:64 of the descriptor.
    pub fn high(&self) -> u64 {
        u64::from(self.dw2) | (u64::from(self.dw3) << 32)
    }
}

/// A 256-bit descriptor, including the reserved upper half (§6.5.2).
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct InvalidationDescriptor256 {
    /// Common header and type-specific fields.
    pub lower: InvalidationDescriptor,
    /// Reserved for all descriptor types implemented here.
    pub upper: [u64; 2],
}

/// Width of an entry, distinct from IQH/IQT's fixed 16-byte register units.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DescriptorWidth {
    /// IQA.DW=0.
    Bits128,
    /// IQA.DW=1.
    Bits256,
}

impl DescriptorWidth {
    /// Entry size in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Bits128 => 16,
            Self::Bits256 => 32,
        }
    }

    pub(crate) fn checked(
        dw: bool,
        mode: TranslationTableMode,
        ecap: EcapReg,
    ) -> Result<Self, InvalidationError> {
        match mode {
            TranslationTableMode::LEGACY => {}
            TranslationTableMode::SCALABLE if ecap.smts() => {}
            TranslationTableMode::ABORT_DMA if ecap.adms() => {}
            _ => return Err(InvalidationError::InvalidMode(mode)),
        }
        if dw {
            if !ecap.smts() && !ecap.adms() {
                return Err(InvalidationError::InvalidWidth);
            }
            Ok(Self::Bits256)
        } else if mode == TranslationTableMode::LEGACY {
            Ok(Self::Bits128)
        } else {
            Err(InvalidationError::InvalidWidth)
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InvalidationError {
    #[error("invalid descriptor width for the translation mode/capabilities")]
    InvalidWidth,
    #[error("invalid translation table mode {0:?}")]
    InvalidMode(TranslationTableMode),
    #[error("invalid or unsupported descriptor type {0:?}")]
    InvalidType(DescriptorType),
    #[error("reserved bits {bits:#x} in descriptor word {word}")]
    ReservedBits { word: usize, bits: u64 },
    #[error("invalid invalidation granularity {0}")]
    InvalidGranularity(u8),
    #[error("page-selective IOTLB invalidation is unsupported")]
    PageInvalidationUnsupported,
    #[error("unsupported IOTLB address mask {0}")]
    AddressMask(u8),
    #[error("unsupported interrupt index mask {0}")]
    IndexMask(u8),
    #[error("invalid queue base or reserved IQA bits")]
    QueueAddress,
    #[error("invalid queue head offset {0:#x}")]
    QueueHead(u64),
    #[error("invalid queue tail offset {0:#x}")]
    QueueTail(u64),
}

impl InvalidationDescriptor256 {
    /// Validate the uncached emulator's descriptor subset. Capability arguments
    /// are internal policy inputs, not an externally selectable feature profile.
    pub(crate) fn validate(
        &self,
        width: DescriptorWidth,
        mode: TranslationTableMode,
        cap: CapReg,
        ecap: EcapReg,
    ) -> Result<(), InvalidationError> {
        DescriptorWidth::checked(width == DescriptorWidth::Bits256, mode, ecap)?;
        let dt = self.lower.descriptor_type();
        // Table 23: only 1..=5 in legacy mode, 1..=9 in scalable/abort mode.
        if !(1..=9).contains(&dt.0) || (mode == TranslationTableMode::LEGACY && dt.0 > 5) {
            return Err(InvalidationError::InvalidType(dt));
        }
        // ATS and PRI are not implemented or advertised. In particular,
        // §§6.5.2.5-.6 require an invalid-type error for Device-TLB commands
        // when ECAP.DT=0; these must not complete as cache no-ops.
        if matches!(
            dt,
            DescriptorType::DEVICE_TLB_INVALIDATE
                | DescriptorType::PASID_DEVICE_TLB_INVALIDATE
                | DescriptorType::PAGE_GROUP_RESPONSE
        ) || (dt == DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE && !ecap.ir())
        {
            return Err(InvalidationError::InvalidType(dt));
        }

        let lo = self.lower.low();
        let hi = self.lower.high();
        let (lo_mask, hi_mask) = match dt {
            DescriptorType::CONTEXT_CACHE_INVALIDATE => (0x0003_ffff_ffff_003f, 0),
            DescriptorType::IOTLB_INVALIDATE => (0x0000_0000_ffff_00ff, 0xffff_ffff_ffff_f07f),
            DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE => (0x0000_ffff_f800_001f, 0),
            DescriptorType::INVALIDATION_WAIT => {
                // SW/IF independently select notifications; FN controls ordering
                // (§§6.5.2.8 and 6.5.2.11). We accept Type 5 with all three
                // clear: no notification or fence, not an invalid Type 0.
                // §7.10 gives a fence-only example, not a minimum-flag rule.
                let mask = if ecap.pds() { 0xff } else { 0x7f };
                (0xffff_ffff_0000_0000 | mask, 0xffff_ffff_ffff_fffc)
            }
            DescriptorType::PASID_IOTLB_INVALIDATE => {
                (0x000f_ffff_ffff_003f, 0xffff_ffff_ffff_f07f)
            }
            DescriptorType::PASID_CACHE_INVALIDATE => (0x000f_ffff_ffff_003f, 0),
            _ => return Err(InvalidationError::InvalidType(dt)),
        };
        for (word, (value, allowed)) in [
            (lo, lo_mask),
            (hi, hi_mask),
            (self.upper[0], 0),
            (self.upper[1], 0),
        ]
        .into_iter()
        .enumerate()
        {
            let bits = value & !allowed;
            if bits != 0 {
                return Err(InvalidationError::ReservedBits { word, bits });
            }
        }

        let granularity = ((lo >> 4) & 3) as u8;
        match dt {
            DescriptorType::CONTEXT_CACHE_INVALIDATE | DescriptorType::IOTLB_INVALIDATE
                if granularity == 0 =>
            {
                return Err(InvalidationError::InvalidGranularity(granularity));
            }
            DescriptorType::PASID_CACHE_INVALIDATE if granularity == 2 => {
                return Err(InvalidationError::InvalidGranularity(granularity));
            }
            DescriptorType::PASID_IOTLB_INVALIDATE if granularity < 2 => {
                return Err(InvalidationError::InvalidGranularity(granularity));
            }
            _ => {}
        }
        if dt == DescriptorType::IOTLB_INVALIDATE && granularity == 3 {
            if !cap.psi() {
                return Err(InvalidationError::PageInvalidationUnsupported);
            }
            let am = IotlbInvalidateAddress::from(hi).am();
            if am > cap.mamv() {
                return Err(InvalidationError::AddressMask(am));
            }
        }
        // CAP.PSI/MAMV do not restrict P_IOTLB (§11.4.2). Nonmatching
        // first-stage invalidations remain valid in a second-stage-only profile.
        if dt == DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE {
            let iec = InterruptCacheInvalidateDw0Dw1::from(lo);
            if iec.granularity() && iec.im() > ecap.mhmv() {
                return Err(InvalidationError::IndexMask(iec.im()));
            }
        }
        Ok(())
    }
}

open_enum! {
    /// Invalidation descriptor types (§6.5).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum DescriptorType: u8 {
        /// Context-Cache Invalidation Descriptor (§6.5.2.1).
        CONTEXT_CACHE_INVALIDATE        = 0x01,
        /// IOTLB Invalidation Descriptor (§6.5.2.3).
        IOTLB_INVALIDATE                = 0x02,
        /// Device-TLB Invalidation Descriptor (§6.5.2.5, not supported).
        DEVICE_TLB_INVALIDATE           = 0x03,
        /// Interrupt Entry Cache Invalidation Descriptor (§6.5.2.7).
        INTERRUPT_ENTRY_CACHE_INVALIDATE = 0x04,
        /// Invalidation Wait Descriptor (§6.5.2.8).
        INVALIDATION_WAIT               = 0x05,
        /// PASID-based IOTLB Invalidation Descriptor (§6.5.2.4).
        PASID_IOTLB_INVALIDATE           = 0x06,
        /// PASID-cache Invalidation Descriptor (§6.5.2.2).
        PASID_CACHE_INVALIDATE           = 0x07,
        /// PASID-based Device-TLB Invalidation Descriptor (§6.5.2.6, unsupported).
        PASID_DEVICE_TLB_INVALIDATE      = 0x08,
        /// Page Group Response Descriptor (§7.6.1, unsupported).
        PAGE_GROUP_RESPONSE             = 0x09,
    }
}

/// Invalidation Wait Descriptor (type 0x05, §6.5.2.8).
///
/// SW and IF independently request completion notifications; both may be
/// disabled. FN controls following-descriptor ordering, not notification
/// (§6.5.2.11). This implementation accepts SW=IF=FN=0 as a wait without
/// notification or fence; §7.10 also gives an explicit fence-only example.
///
/// ```text
/// Bits [3:0]   = Type (0x05)
/// Bit  [4]     = IF (Interrupt Flag — generate invalidation completion event)
/// Bit  [5]     = SW (Status Write — write status data to status address)
/// Bit  [6]     = FN (Fence — complete this wait before following descriptors)
/// Bit  [7]     = PD (Page-request Drain)
/// Bit  [8]     = reserved
/// Bits [11:9]  = Type[6:4] (zero)
/// Bits [31:12] = reserved
/// Bits [63:32] = Status Data (32-bit value to write)
/// Bits [65:64] = reserved
/// Bits [127:66]= Status Address [63:2] (DWORD-aligned)
/// ```
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidationWaitDw0Dw1 {
    /// Descriptor type (must be 0x05).
    #[bits(4)]
    pub desc_type: u8,
    /// Interrupt Flag — 1 = generate invalidation completion event; 0 = no event.
    pub iflag: bool,
    /// Status Write — 1 = write status_data to status_address; 0 = ignore both.
    pub sw: bool,
    /// Fence — 1 = complete this wait before executing following descriptors.
    /// If 0, following descriptors may execute before this wait completes.
    pub fn_flag: bool,
    /// Page-request Drain.
    pub pd: bool,
    #[bits(1)]
    _reserved1: u64,
    /// Upper three bits of the descriptor type (zero for this type).
    #[bits(3)]
    pub type_hi: u8,
    #[bits(20)]
    _reserved2: u64,
    /// Status Data (32-bit value to write when SW=1).
    #[bits(32)]
    pub status_data: u32,
}

/// Invalidation Wait Descriptor — high 64 bits.
///
/// Contains the status address (bits 127:66 = address bits 63:2).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InvalidationWaitDw2Dw3 {
    #[bits(2)]
    _reserved: u64,
    /// Status Address bits [63:2]. The full address is `sal << 2`.
    #[bits(62)]
    pub sal: u64,
}

impl InvalidationWaitDw2Dw3 {
    /// Get the full status address (DWORD-aligned).
    pub fn status_address(&self) -> u64 {
        self.sal() << 2
    }
}

/// Parse a `InvalidationDescriptor` as INVALIDATION_WAIT fields.
pub fn parse_invalidation_wait(
    desc: &InvalidationDescriptor,
) -> (InvalidationWaitDw0Dw1, InvalidationWaitDw2Dw3) {
    let lo = desc.low();
    let hi = desc.high();
    (
        InvalidationWaitDw0Dw1::from(lo),
        InvalidationWaitDw2Dw3::from(hi),
    )
}

/// Context-Cache Invalidation Descriptor (type 0x01, §6.5.2.1).
///
/// ```text
/// Bits [3:0]   = Type (0x01)
/// Bits [5:4]   = Granularity (01=global, 10=domain, 11=device)
/// Bits [8:6]   = reserved
/// Bits [11:9]  = Type[6:4] (zero)
/// Bits [15:12] = reserved
/// Bits [31:16] = Domain ID (for domain/device granularity)
/// Bits [47:32] = Source ID (for device granularity)
/// Bits [49:48] = Function Mask (for device granularity)
/// Bits [63:50] = reserved
/// Bits [127:64]= reserved
/// ```
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct ContextCacheInvalidateDw0Dw1 {
    /// Descriptor type (must be 0x01).
    #[bits(4)]
    pub desc_type: u8,
    /// Invalidation granularity: 01=global, 10=domain, 11=device.
    #[bits(2)]
    pub granularity: u8,
    #[bits(3)]
    _reserved1: u64,
    /// Upper three bits of the descriptor type.
    #[bits(3)]
    pub type_hi: u8,
    #[bits(4)]
    _reserved_type: u64,
    /// Domain ID (for domain-selective and device-selective invalidation).
    #[bits(16)]
    pub did: u16,
    /// Source ID (for device-selective invalidation).
    #[bits(16)]
    pub sid: u16,
    /// Function Mask for device-selective invalidation.
    #[bits(2)]
    pub fm: u8,
    #[bits(14)]
    _reserved2: u64,
}

/// IOTLB Invalidation Descriptor (type 0x02, §6.5.2.3).
///
/// ```text
/// Bits [3:0]   = Type (0x02)
/// Bits [5:4]   = Granularity (01=global, 10=domain, 11=page)
/// Bit  [6]     = DW (Drain Writes)
/// Bit  [7]     = DR (Drain Reads)
/// Bit  [8]     = reserved
/// Bits [11:9]  = Type[6:4] (zero)
/// Bits [15:12] = reserved
/// Bits [31:16] = Domain ID (for domain/page granularity)
/// Bits [63:32] = reserved
/// ```
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IotlbInvalidateDw0Dw1 {
    /// Descriptor type (must be 0x02).
    #[bits(4)]
    pub desc_type: u8,
    /// Invalidation granularity: 01=global, 10=domain, 11=page.
    #[bits(2)]
    pub granularity: u8,
    /// Drain Writes.
    pub dw: bool,
    /// Drain Reads.
    pub dr: bool,
    #[bits(1)]
    _reserved1: u64,
    /// Upper three bits of the descriptor type.
    #[bits(3)]
    pub type_hi: u8,
    #[bits(4)]
    _reserved_type: u64,
    /// Domain ID (for domain-selective and page-selective invalidation).
    #[bits(16)]
    pub did: u16,
    #[bits(32)]
    _reserved2: u64,
}

/// Address fields shared by IOTLB and P_IOTLB invalidations (§§6.5.2.3-.4).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct IotlbInvalidateAddress {
    /// Number of low address bits (starting at bit 12) to mask.
    #[bits(6)]
    pub am: u8,
    /// Invalidation hint, ignored for non-page-selective requests.
    pub ih: bool,
    #[bits(5)]
    _reserved: u64,
    /// Input address bits 63:12.
    #[bits(52)]
    pub address: u64,
}

/// Common low word of PASID-cache and P_IOTLB descriptors (§§6.5.2.2, 6.5.2.4).
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct PasidInvalidateDw0Dw1 {
    /// Low four type bits (7 for PASID-cache, 6 for P_IOTLB).
    #[bits(4)]
    pub desc_type: u8,
    /// PASID-cache: 0=domain, 1=PASID, 3=global; P_IOTLB: 2=PASID, 3=page.
    #[bits(2)]
    pub granularity: u8,
    #[bits(3)]
    _reserved1: u64,
    /// Upper three type bits.
    #[bits(3)]
    pub type_hi: u8,
    #[bits(4)]
    _reserved2: u64,
    /// Domain ID.
    pub did: u16,
    /// PASID, including an implied RID_PASID.
    #[bits(20)]
    pub pasid: u32,
    #[bits(12)]
    _reserved3: u64,
}

/// Interrupt Entry Cache Invalidation Descriptor (type 0x04, §6.5.2.7).
///
/// ```text
/// Bits [3:0]   = Type (0x04)
/// Bit  [4]     = Granularity (0=global, 1=index-selective)
/// Bits [8:5]   = reserved
/// Bits [11:9]  = Type[6:4] (zero)
/// Bits [26:12] = reserved
/// Bits [31:27] = IM (Index Mask, for index-selective)
/// Bits [47:32] = IIDX (Interrupt Index, for index-selective)
/// Bits [63:48] = reserved
/// Bits [127:64]= reserved
/// ```
#[bitfield(u64)]
#[derive(Inspect)]
#[rustfmt::skip]
pub struct InterruptCacheInvalidateDw0Dw1 {
    /// Descriptor type (must be 0x04).
    #[bits(4)]
    pub desc_type: u8,
    /// Invalidation granularity: 0=global, 1=index-selective.
    pub granularity: bool,
    #[bits(4)]
    _reserved1: u64,
    /// Upper three bits of the descriptor type.
    #[bits(3)]
    pub type_hi: u8,
    #[bits(15)]
    _reserved2: u64,
    /// Index Mask (5 bits, for index-selective invalidation).
    #[bits(5)]
    pub im: u8,
    /// Interrupt Index (16 bits, for index-selective invalidation).
    #[bits(16)]
    pub iidx: u16,
    #[bits(16)]
    _reserved3: u64,
}

/// Parse an `InvalidationDescriptor` as INTERRUPT_ENTRY_CACHE_INVALIDATE fields.
pub fn parse_interrupt_cache_invalidate(
    desc: &InvalidationDescriptor,
) -> InterruptCacheInvalidateDw0Dw1 {
    InterruptCacheInvalidateDw0Dw1::from(desc.low())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn raw(lo: u64, hi: u64) -> InvalidationDescriptor256 {
        InvalidationDescriptor256 {
            lower: InvalidationDescriptor {
                dw0: lo as u32,
                dw1: (lo >> 32) as u32,
                dw2: hi as u32,
                dw3: (hi >> 32) as u32,
            },
            upper: [0; 2],
        }
    }

    fn cap() -> CapReg {
        CapReg::new().with_psi(true).with_mamv(18)
    }

    fn ecap() -> EcapReg {
        EcapReg::new().with_smts(true).with_ir(true).with_mhmv(15)
    }

    fn validate(desc: &InvalidationDescriptor256) -> Result<(), InvalidationError> {
        desc.validate(
            DescriptorWidth::Bits256,
            TranslationTableMode::SCALABLE,
            cap(),
            ecap(),
        )
    }

    #[test]
    fn test_full_type_encoding() {
        assert_eq!(size_of::<InvalidationDescriptor>(), 16);
        assert_eq!(size_of::<InvalidationDescriptor256>(), 32);
        for dt in 0..128 {
            let lo = (dt & 15) | ((dt >> 4) << 9);
            let desc = raw(lo, 0);
            assert_eq!(desc.lower.descriptor_type().0, dt as u8);
            for bit in [4, 5, 6, 7, 8, 12, 13, 14, 15, 31, 32, 63] {
                assert_eq!(raw(lo | (1 << bit), 0).lower.descriptor_type().0, dt as u8);
            }
        }
        for (dt, value) in [
            (DescriptorType::CONTEXT_CACHE_INVALIDATE, 1),
            (DescriptorType::IOTLB_INVALIDATE, 2),
            (DescriptorType::DEVICE_TLB_INVALIDATE, 3),
            (DescriptorType::INTERRUPT_ENTRY_CACHE_INVALIDATE, 4),
            (DescriptorType::INVALIDATION_WAIT, 5),
            (DescriptorType::PASID_IOTLB_INVALIDATE, 6),
            (DescriptorType::PASID_CACHE_INVALIDATE, 7),
            (DescriptorType::PASID_DEVICE_TLB_INVALIDATE, 8),
            (DescriptorType::PAGE_GROUP_RESPONSE, 9),
        ] {
            assert_eq!(dt.0, value);
        }
    }

    #[test]
    fn test_literal_descriptor_bytes() {
        let bytes = [
            0x26, 0x00, 0x34, 0x12, 0xde, 0xbc, 0x0a, 0x00, 0x52, 0x10, 0x32, 0x54, 0x76, 0x98,
            0xba, 0xdc, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let desc = InvalidationDescriptor256::read_from_bytes(&bytes).unwrap();
        assert_eq!(desc.lower.low(), 0x000a_bcde_1234_0026);
        assert_eq!(desc.lower.high(), 0xdcba_9876_5432_1052);
        assert_eq!(desc.upper, [0, 0]);
        assert_eq!(desc.as_bytes(), bytes);
        let short = InvalidationDescriptor::read_from_bytes(&bytes[..16]).unwrap();
        assert_eq!(short.as_bytes(), &bytes[..16]);
        assert_eq!(
            short.descriptor_type(),
            DescriptorType::PASID_IOTLB_INVALIDATE
        );
        assert!(validate(&desc).is_ok());
    }

    #[test]
    fn test_context_raw_fields() {
        let value = 0x0003_abcd_9876_0031;
        let cc = ContextCacheInvalidateDw0Dw1::from(value);
        assert_eq!((cc.desc_type(), cc.type_hi(), cc.granularity()), (1, 0, 3));
        assert_eq!((cc.did(), cc.sid(), cc.fm()), (0x9876, 0xabcd, 3));
        assert_eq!(
            ContextCacheInvalidateDw0Dw1::new()
                .with_desc_type(1)
                .with_granularity(3)
                .with_did(0x9876)
                .with_sid(0xabcd)
                .with_fm(3)
                .into_bits(),
            value
        );
    }

    #[test]
    fn test_iotlb_raw_fields() {
        for g in 0..4 {
            for drains in 0..4 {
                let value = 0xabcd_0002 | (g << 4) | (drains << 6);
                let iotlb = IotlbInvalidateDw0Dw1::from(value);
                assert_eq!(iotlb.granularity(), g as u8);
                assert_eq!(iotlb.dw(), drains & 1 != 0);
                assert_eq!(iotlb.dr(), drains & 2 != 0);
                assert_eq!(iotlb.did(), 0xabcd);
                assert_eq!(
                    IotlbInvalidateDw0Dw1::new()
                        .with_desc_type(2)
                        .with_granularity(g as u8)
                        .with_dw(drains & 1 != 0)
                        .with_dr(drains & 2 != 0)
                        .with_did(0xabcd)
                        .into_bits(),
                    value
                );
            }
        }
        for am in 0..64 {
            for ih in [false, true] {
                let value = 0x1234_5678_9abc_d000 | am | ((ih as u64) << 6);
                let address = IotlbInvalidateAddress::from(value);
                assert_eq!(
                    (address.am(), address.ih(), address.address()),
                    (am as u8, ih, 0x0001_2345_6789_abcd)
                );
                assert_eq!(
                    IotlbInvalidateAddress::new()
                        .with_am(am as u8)
                        .with_ih(ih)
                        .with_address(0x0001_2345_6789_abcd)
                        .into_bits(),
                    value
                );
            }
        }
    }

    #[test]
    fn test_pasid_raw_fields() {
        for dt in [6, 7] {
            for g in 0..4 {
                let value = 0x000a_bcde_1234_0000 | (g << 4) | dt;
                let pasid = PasidInvalidateDw0Dw1::from(value);
                assert_eq!(
                    (pasid.desc_type(), pasid.type_hi(), pasid.granularity()),
                    (dt as u8, 0, g as u8)
                );
                assert_eq!((pasid.pasid(), pasid.did()), (0xabcde, 0x1234));
                assert_eq!(
                    PasidInvalidateDw0Dw1::new()
                        .with_desc_type(dt as u8)
                        .with_granularity(g as u8)
                        .with_pasid(0xabcde)
                        .with_did(0x1234)
                        .into_bits(),
                    value
                );
            }
        }
    }

    #[test]
    fn test_interrupt_cache_raw_fields() {
        for im in 0..32 {
            for g in [false, true] {
                let value = 0x0000_abcd_0000_0004 | (im << 27) | ((g as u64) << 4);
                let iec = parse_interrupt_cache_invalidate(&raw(value, 0).lower);
                assert_eq!(
                    (iec.im(), iec.iidx(), iec.granularity()),
                    (im as u8, 0xabcd, g)
                );
                assert_eq!(
                    InterruptCacheInvalidateDw0Dw1::new()
                        .with_desc_type(4)
                        .with_granularity(g)
                        .with_im(im as u8)
                        .with_iidx(0xabcd)
                        .into_bits(),
                    value
                );
            }
        }
    }

    #[test]
    fn test_wait_raw_fields() {
        for flags in 0..16 {
            let value = 0xabcd_1234_0000_0005 | (flags << 4);
            let (lo, hi) = parse_invalidation_wait(&raw(value, 0x1234_5678_9abc_defc).lower);
            assert_eq!(lo.iflag(), flags & 1 != 0);
            assert_eq!(lo.sw(), flags & 2 != 0);
            assert_eq!(lo.fn_flag(), flags & 4 != 0);
            assert_eq!(lo.pd(), flags & 8 != 0);
            assert_eq!(lo.status_data(), 0xabcd_1234);
            assert_eq!(hi.status_address(), 0x1234_5678_9abc_defc);
            assert_eq!(
                InvalidationWaitDw0Dw1::new()
                    .with_desc_type(5)
                    .with_iflag(flags & 1 != 0)
                    .with_sw(flags & 2 != 0)
                    .with_fn_flag(flags & 4 != 0)
                    .with_pd(flags & 8 != 0)
                    .with_status_data(0xabcd_1234)
                    .into_bits(),
                value
            );
            assert_eq!(
                InvalidationWaitDw2Dw3::new()
                    .with_sal(0x1234_5678_9abc_defc >> 2)
                    .into_bits(),
                0x1234_5678_9abc_defc
            );
        }
    }

    #[test]
    fn test_mode_width_type_matrix() {
        for smts in [false, true] {
            for adms in [false, true] {
                let ecap = ecap().with_smts(smts).with_adms(adms);
                for mode in 0..4 {
                    for dw in [false, true] {
                        let mode_allowed = match mode {
                            0 => true,
                            1 => smts,
                            3 => adms,
                            _ => false,
                        };
                        let width_allowed = if dw { smts || adms } else { mode == 0 };
                        let width = if dw {
                            DescriptorWidth::Bits256
                        } else {
                            DescriptorWidth::Bits128
                        };
                        assert_eq!(
                            DescriptorWidth::checked(dw, TranslationTableMode(mode), ecap).is_ok(),
                            mode_allowed && width_allowed
                        );
                        for dt in 0..128 {
                            let g = if dt == 6 { 2 } else { 1 };
                            let lo = (dt & 15) | ((dt >> 4) << 9) | (g << 4);
                            let expected = mode_allowed
                                && width_allowed
                                && (matches!(dt, 1 | 2 | 4 | 5)
                                    || (mode != 0 && matches!(dt, 6 | 7)));
                            assert_eq!(
                                raw(lo, 0)
                                    .validate(width, TranslationTableMode(mode), cap(), ecap)
                                    .is_ok(),
                                expected,
                                "smts={smts} adms={adms} mode={mode} dw={dw} type={dt}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_granularity_and_ignored_fields() {
        for (dt, valid) in [
            (1, [false, true, true, true]),
            (2, [false, true, true, true]),
            (6, [false, false, true, true]),
            (7, [true, true, false, true]),
        ] {
            for (g, expected) in valid.into_iter().enumerate() {
                assert_eq!(validate(&raw(dt | ((g as u64) << 4), 0)).is_ok(), expected);
            }
        }
        // Ignore DID/SID/FM for global CC; DID/PASID in global PC; the full
        // 20-bit PASID is legal even though explicit requests-with-PASID are off.
        for lo in [
            0x0003_ffff_ffff_0011,
            0x000f_ffff_ffff_0037,
            0x000f_ffff_ffff_0007,
        ] {
            assert!(validate(&raw(lo, 0)).is_ok());
        }
        // Address, AM and IH are ignored for global/domain IOTLB and PASID-
        // selective P_IOTLB. Address bits above MGAW are ignored, not reserved.
        for lo in [0xffff_0012, 0xffff_0022, 0x000f_ffff_ffff_0026] {
            assert!(validate(&raw(lo, 0xffff_ffff_ffff_f07f)).is_ok());
        }
        // SW=0 ignores status data/address, but not the reserved alignment bits.
        assert!(validate(&raw(0xffff_ffff_0000_0005, 0xffff_ffff_ffff_fffc)).is_ok());
        assert!(validate(&raw(5, 1)).is_err());
    }

    #[test]
    fn test_capability_dependent_fields() {
        for am in 0..64 {
            let desc = raw(0x32, 0xffff_ffff_ffff_f000 | am);
            assert_eq!(validate(&desc).is_ok(), am <= 18);
            assert_eq!(
                desc.validate(
                    DescriptorWidth::Bits256,
                    TranslationTableMode::SCALABLE,
                    cap().with_psi(false),
                    ecap()
                ),
                Err(InvalidationError::PageInvalidationUnsupported)
            );
            // Neither PSI nor MAMV restrict P_IOTLB; no shifts by AM are needed.
            assert!(
                raw(0x36, am)
                    .validate(
                        DescriptorWidth::Bits256,
                        TranslationTableMode::SCALABLE,
                        cap().with_psi(false).with_mamv(0),
                        ecap()
                    )
                    .is_ok()
            );
        }
        for im in 0..32 {
            assert_eq!(validate(&raw(0x14 | (im << 27), 0)).is_ok(), im <= 15);
            assert!(validate(&raw(4 | (im << 27), 0)).is_ok());
        }
        assert!(validate(&raw(0x85, 0)).is_err()); // PD without PDS.
        assert!(
            raw(0x85, 0)
                .validate(
                    DescriptorWidth::Bits256,
                    TranslationTableMode::SCALABLE,
                    cap(),
                    ecap().with_pds(true)
                )
                .is_ok()
        );
        assert!(
            raw(4, 0)
                .validate(
                    DescriptorWidth::Bits256,
                    TranslationTableMode::SCALABLE,
                    cap(),
                    ecap().with_ir(false)
                )
                .is_err()
        );
    }

    #[test]
    fn test_every_reserved_bit_and_padding() {
        // Independent bit positions from Figures 6-1 through 6-8.
        for (lo, reserved) in [
            (0x11, vec![6..9, 12..16, 50..128]),
            (0x12, vec![8..9, 12..16, 32..64, 71..76]),
            (0x04, vec![5..9, 12..27, 48..128]),
            (0x05, vec![7..9, 12..32, 64..66]),
            (0x26, vec![6..9, 12..16, 52..64, 71..76]),
            (0x07, vec![6..9, 12..16, 52..128]),
        ] {
            for bit in reserved.into_iter().flatten().chain(128..256) {
                let mut desc = raw(lo, 0);
                match bit {
                    0..64 => desc.lower = raw(lo | (1 << bit), 0).lower,
                    64..128 => desc.lower = raw(lo, 1 << (bit - 64)).lower,
                    _ => desc.upper[(bit - 128) / 64] = 1 << (bit % 64),
                }
                assert!(
                    matches!(validate(&desc), Err(InvalidationError::ReservedBits { .. })),
                    "type={} bit={bit}",
                    lo & 15
                );
            }
        }
    }

    #[test]
    fn test_parse_invalidation_wait() {
        let lo_val = InvalidationWaitDw0Dw1::new()
            .with_desc_type(0x05)
            .with_sw(true)
            .with_status_data(0x42);
        let hi_val = InvalidationWaitDw2Dw3::new().with_sal(0x1000);

        let lo_raw = u64::from(lo_val);
        let hi_raw = u64::from(hi_val);

        let desc = InvalidationDescriptor {
            dw0: lo_raw as u32,
            dw1: (lo_raw >> 32) as u32,
            dw2: hi_raw as u32,
            dw3: (hi_raw >> 32) as u32,
        };

        let (parsed_lo, parsed_hi) = parse_invalidation_wait(&desc);
        assert!(parsed_lo.sw());
        assert_eq!(parsed_lo.status_data(), 0x42);
        assert_eq!(parsed_hi.status_address(), 0x1000 << 2);
    }

    #[test]
    fn test_parse_interrupt_cache_invalidate() {
        let lo_val = InterruptCacheInvalidateDw0Dw1::new()
            .with_desc_type(0x04)
            .with_granularity(true)
            .with_im(0x1f)
            .with_iidx(0x1234);

        let lo_raw = u64::from(lo_val);
        let desc = InvalidationDescriptor {
            dw0: lo_raw as u32,
            dw1: (lo_raw >> 32) as u32,
            dw2: 0,
            dw3: 0,
        };

        let parsed = parse_interrupt_cache_invalidate(&desc);
        assert!(parsed.granularity());
        assert_eq!(parsed.im(), 0x1f);
        assert_eq!(parsed.iidx(), 0x1234);
    }
}
