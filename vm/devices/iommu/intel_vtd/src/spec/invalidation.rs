// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Invalidation queue descriptor types for the Intel VT-d IOMMU.
//!
//! Based on Intel VT-d Specification Rev 4.1, §6.5. Invalidation descriptors
//! are 128-bit (16-byte) entries in a circular queue. The descriptor type is
//! determined by bits 3:0 of the first dword.

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
    /// First dword: bits 3:0 = descriptor type, rest is type-dependent.
    pub dw0: u32,
    /// Second dword: type-dependent fields.
    pub dw1: u32,
    /// Third dword: type-dependent fields.
    pub dw2: u32,
    /// Fourth dword: type-dependent fields.
    pub dw3: u32,
}

impl InvalidationDescriptor {
    /// Extract the 4-bit descriptor type from bits 3:0 of dw0.
    pub fn descriptor_type(&self) -> DescriptorType {
        DescriptorType((self.dw0 & 0xF) as u8)
    }
}

open_enum! {
    /// Invalidation descriptor types (§6.5).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum DescriptorType: u8 {
        /// Context-Cache Invalidation Descriptor (§6.5.2.1).
        CONTEXT_CACHE_INVALIDATE        = 0x01,
        /// IOTLB Invalidation Descriptor (§6.5.2.2).
        IOTLB_INVALIDATE                = 0x02,
        /// Device-TLB Invalidation Descriptor (§6.5.2.3, not supported).
        DEVICE_TLB_INVALIDATE           = 0x03,
        /// Interrupt Entry Cache Invalidation Descriptor (§6.5.2.6).
        INTERRUPT_ENTRY_CACHE_INVALIDATE = 0x04,
        /// Invalidation Wait Descriptor (§6.5.2.8).
        INVALIDATION_WAIT               = 0x05,
    }
}

/// Invalidation Wait Descriptor (type 0x05, §6.5.2.8).
///
/// ```text
/// Bits [3:0]   = Type (0x05)
/// Bit  [4]     = IF (Interrupt Flag — generate invalidation completion event)
/// Bit  [5]     = SW (Status Write — write status data to status address)
/// Bit  [6]     = FN (Fence — ensure prior descriptors complete first)
/// Bit  [7]     = PD (Page-request Drain)
/// Bits [31:8]  = reserved
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
    /// Interrupt Flag — 1 = generate invalidation completion event.
    pub iflag: bool,
    /// Status Write — 1 = write status_data to status_address.
    pub sw: bool,
    /// Fence — 1 = ensure all prior invalidation descriptors complete first.
    pub fn_flag: bool,
    /// Page-request Drain.
    pub pd: bool,
    #[bits(24)]
    _reserved1: u64,
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
    let lo = ((desc.dw1 as u64) << 32) | desc.dw0 as u64;
    let hi = ((desc.dw3 as u64) << 32) | desc.dw2 as u64;
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
/// Bits [15:6]  = reserved
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
    #[bits(10)]
    _reserved1: u64,
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
/// Bit  [4]     = reserved
/// Bits [6:5]   = Granularity (01=global, 10=domain, 11=page)
/// Bit  [7]     = DW (Drain Writes)
/// Bit  [8]     = DR (Drain Reads)
/// Bits [15:9]  = reserved
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
    #[bits(1)]
    _reserved_bit4: u64,
    /// Invalidation granularity: 01=global, 10=domain, 11=page.
    #[bits(2)]
    pub granularity: u8,
    /// Drain Writes.
    pub dw: bool,
    /// Drain Reads.
    pub dr: bool,
    #[bits(7)]
    _reserved1: u64,
    /// Domain ID (for domain-selective and page-selective invalidation).
    #[bits(16)]
    pub did: u16,
    #[bits(32)]
    _reserved2: u64,
}

/// Interrupt Entry Cache Invalidation Descriptor (type 0x04, §6.5.2.7).
///
/// ```text
/// Bits [3:0]   = Type (0x04)
/// Bit  [4]     = Granularity (0=global, 1=index-selective)
/// Bits [22:5]  = reserved
/// Bits [27:23] = IM (Index Mask, for index-selective)
/// Bits [31:28] = reserved
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
    #[bits(18)]
    _reserved1: u64,
    /// Index Mask (5 bits, for index-selective invalidation).
    #[bits(5)]
    pub im: u8,
    #[bits(4)]
    _reserved2: u64,
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
    let lo = ((desc.dw1 as u64) << 32) | desc.dw0 as u64;
    InterruptCacheInvalidateDw0Dw1::from(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

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
