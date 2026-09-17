// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL Early Discovery Table (CEDT) builder.

use core::mem::size_of;
use cxl_spec::spec::CEDT_STRUCTURE_TYPE_CFMWS;
use cxl_spec::spec::CEDT_STRUCTURE_TYPE_CHBS;
use cxl_spec::spec::CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES;
use cxl_spec::spec::CXL_HPA_ALIGNMENT;
use cxl_spec::spec::InterleaveArithmetic;
use cxl_spec::spec::InterleaveGranularity;
use cxl_spec::spec::InterleaveWays;
use memory_range::MemoryRange;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Validation errors when adding CHBS/CFMWS CXL host-bridge entries.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CedtHostBridgeError {
    /// HDM range is empty.
    #[error("HDM range must be non-empty")]
    EmptyHdmRange,
    /// CHBCR range is empty.
    #[error("CHBCR range must be non-empty")]
    EmptyChbcrRange,
    /// CHBCR range length does not match the spec-defined size.
    #[error("CHBCR range has invalid length {actual:#x}, expected {expected:#x}")]
    InvalidChbcrRangeLength { actual: u64, expected: u64 },
    /// CHBCR base is not aligned to the CHBCR aperture size.
    #[error("CHBCR base {base:#x} is not aligned to {alignment:#x}")]
    InvalidChbcrBaseAlignment { base: u64, alignment: u64 },
    /// HDM base is not aligned to CXL HPA alignment.
    #[error("HDM base {base:#x} is not aligned to {alignment:#x}")]
    InvalidHdmBaseAlignment { base: u64, alignment: u64 },
    /// HDM size is not aligned to CXL HPA alignment.
    #[error("HDM size {size:#x} is not aligned to {alignment:#x}")]
    InvalidHdmSizeAlignment { size: u64, alignment: u64 },
}

/// Errors while serializing CEDT bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CedtSerializeError {
    /// Serialized CEDT size exceeded ACPI header u32 length field capacity.
    #[error("CEDT length {actual:#x} exceeds u32 length field")]
    LengthOverflow { actual: usize },
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DescriptionHeader {
    pub signature: u32,
    _length: u32, // placeholder, filled in during serialization to bytes
    pub revision: u8,
    _checksum: u8, // placeholder, filled in during serialization to bytes
    pub oem_id: [u8; 6],
    pub oem_table_id: u64,
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_rev: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ChbsStructure {
    structure_type: u8,
    reserved0: u8,
    record_length: u16,
    uid: u32,
    cxl_version: u32,
    reserved1: u32,
    base: u64,
    length: u64,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct CfmwsStructure {
    structure_type: u8,
    reserved0: u8,
    record_length: u16,
    reserved1: u32,
    base_hpa: u64,
    window_size: u64,
    eniw: u8,
    interleave_arithmetic: u8,
    reserved2: u16,
    hbig: u32,
    window_restrictions: u16,
    qtg_id: u16,
}

/// CXL Early Discovery Table (CEDT).
pub struct Cedt {
    description_header: DescriptionHeader,
    structures: Vec<u8>,
}

impl Cedt {
    /// Create an empty CEDT.
    pub fn new() -> Self {
        Self {
            description_header: DescriptionHeader {
                signature: u32::from_le_bytes(*b"CEDT"),
                _length: 0,
                revision: 2,
                _checksum: 0,
                oem_id: *b"MSFTVM",
                oem_table_id: u64::from_le_bytes(*b"CEDT01\0\0"),
                oem_revision: 1,
                creator_id: u32::from_le_bytes(*b"MSFT"),
                creator_rev: 0x01000000,
            },
            structures: Vec::new(),
        }
    }

    /// Add one CXL host bridge to CEDT.
    ///
    /// This appends:
    /// 1) One CHBS structure.
    /// 2) One CFMWS structure where only base HPA and window size are set,
    ///    with one Interleave Target List entry for ENIW=0 (1-way interleave, default).
    pub fn add_cxl_host_bridge(
        &mut self,
        uid: u32,
        hdm_range: MemoryRange,
        chbcr_range: MemoryRange,
        hdm_window_restrictions: u16,
    ) -> Result<(), CedtHostBridgeError> {
        if hdm_range.is_empty() {
            return Err(CedtHostBridgeError::EmptyHdmRange);
        }
        if chbcr_range.is_empty() {
            return Err(CedtHostBridgeError::EmptyChbcrRange);
        }
        if chbcr_range.len() != CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES {
            return Err(CedtHostBridgeError::InvalidChbcrRangeLength {
                actual: chbcr_range.len(),
                expected: CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES,
            });
        }
        if !chbcr_range
            .start()
            .is_multiple_of(CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES)
        {
            return Err(CedtHostBridgeError::InvalidChbcrBaseAlignment {
                base: chbcr_range.start(),
                alignment: CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES,
            });
        }
        if !hdm_range.start().is_multiple_of(CXL_HPA_ALIGNMENT) {
            return Err(CedtHostBridgeError::InvalidHdmBaseAlignment {
                base: hdm_range.start(),
                alignment: CXL_HPA_ALIGNMENT,
            });
        }
        if !hdm_range.len().is_multiple_of(CXL_HPA_ALIGNMENT) {
            return Err(CedtHostBridgeError::InvalidHdmSizeAlignment {
                size: hdm_range.len(),
                alignment: CXL_HPA_ALIGNMENT,
            });
        }

        let chbs = ChbsStructure {
            structure_type: CEDT_STRUCTURE_TYPE_CHBS,
            reserved0: 0,
            record_length: size_of::<ChbsStructure>() as u16,
            uid,
            // Host bridge associated with one or more CXL root ports.
            cxl_version: 1,
            reserved1: 0,
            base: chbcr_range.start(),
            length: chbcr_range.len(),
        };
        self.structures.extend_from_slice(chbs.as_bytes());

        let cfmws = CfmwsStructure {
            structure_type: CEDT_STRUCTURE_TYPE_CFMWS,
            reserved0: 0,
            // ENIW=0 means 1-way interleave, so include one 32-bit target entry.
            record_length: (size_of::<CfmwsStructure>() + size_of::<u32>()) as u16,
            reserved1: 0,
            base_hpa: hdm_range.start(),
            window_size: hdm_range.len(),
            eniw: InterleaveWays::WAY_1,
            interleave_arithmetic: InterleaveArithmetic::STANDARD_MODULO,
            reserved2: 0,
            hbig: InterleaveGranularity::BYTES_256 as u32,
            window_restrictions: hdm_window_restrictions,
            qtg_id: 0,
        };
        self.structures.extend_from_slice(cfmws.as_bytes());
        self.structures.extend_from_slice(&uid.to_le_bytes());

        Ok(())
    }

    /// Serialize CEDT to bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, CedtSerializeError> {
        let mut byte_stream = Vec::new();
        byte_stream.extend_from_slice(self.description_header.as_bytes());
        byte_stream.extend_from_slice(&self.structures);

        let length = byte_stream.len();
        let length = u32::try_from(length).map_err(|_| CedtSerializeError::LengthOverflow {
            actual: byte_stream.len(),
        })?;

        let (header, _) = DescriptionHeader::mut_from_prefix(byte_stream.as_mut_slice())
            .expect("serialized CEDT must start with a complete description header");
        header._length = length;
        // Checksum is computed with this field set to zero.
        header._checksum = 0;

        let mut checksum: u8 = 0;
        for byte in &byte_stream {
            checksum = checksum.wrapping_add(*byte);
        }
        DescriptionHeader::mut_from_prefix(byte_stream.as_mut_slice())
            .expect("serialized CEDT must start with a complete description header")
            .0
            ._checksum = (!checksum).wrapping_add(1);

        Ok(byte_stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cedt_header_signature_and_revision() {
        let cedt = Cedt::new();
        let bytes = cedt.to_bytes().expect("serialize CEDT");
        assert_eq!(&bytes[0..4], b"CEDT");
        assert_eq!(bytes[8], 2);
    }

    #[test]
    fn add_cxl_host_bridge_adds_chbs_and_cfmws() {
        let mut cedt = Cedt::new();
        cedt.add_cxl_host_bridge(
            7,
            MemoryRange::new(0x4000_0000..0x8000_0000),
            MemoryRange::new(0x1000_0000..0x1001_0000),
            cxl_spec::spec::CfmwsWindowRestrictions::DEVICE_COHERENT.bits(),
        )
        .expect("valid CXL host bridge ranges");
        let bytes = cedt.to_bytes().expect("serialize CEDT");

        // Header is 36 bytes. First structure should be CHBS type 0.
        assert_eq!(bytes[36], CEDT_STRUCTURE_TYPE_CHBS);
        // CHBS record length should be 0x20.
        assert_eq!(u16::from_le_bytes([bytes[38], bytes[39]]), 0x20);

        // Second structure should be CFMWS type 1 at offset 36 + 0x20.
        assert_eq!(bytes[36 + 0x20], CEDT_STRUCTURE_TYPE_CFMWS);
        // CFMWS record length should be 0x28 (base structure + one target entry).
        assert_eq!(
            u16::from_le_bytes([bytes[36 + 0x20 + 2], bytes[36 + 0x20 + 3]]),
            0x28
        );

        // Interleave Target List[0] should contain the CHBS UID for ENIW=0.
        assert_eq!(
            u32::from_le_bytes([
                bytes[36 + 0x20 + 0x24],
                bytes[36 + 0x20 + 0x25],
                bytes[36 + 0x20 + 0x26],
                bytes[36 + 0x20 + 0x27],
            ]),
            7
        );
    }

    #[test]
    fn add_cxl_host_bridge_rejects_unaligned_chbcr_base() {
        let mut cedt = Cedt::new();
        let err = cedt
            .add_cxl_host_bridge(
                0,
                MemoryRange::new(0x4000_0000..0x5000_0000),
                // 4K-aligned but not 64K-aligned.
                MemoryRange::new(0x1000_1000..0x1001_1000),
                cxl_spec::spec::CfmwsWindowRestrictions::DEVICE_COHERENT.bits(),
            )
            .expect_err("unaligned CHBCR base should be rejected");

        assert_eq!(
            err,
            CedtHostBridgeError::InvalidChbcrBaseAlignment {
                base: 0x1000_1000,
                alignment: CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES,
            }
        );
    }

    #[test]
    fn add_cxl_host_bridge_rejects_unaligned_hdm_base() {
        let mut cedt = Cedt::new();
        let err = cedt
            .add_cxl_host_bridge(
                0,
                // 4K-aligned but not 256MiB-aligned.
                MemoryRange::new(0x4000_1000..0x5000_1000),
                MemoryRange::new(0x1000_0000..0x1001_0000),
                cxl_spec::spec::CfmwsWindowRestrictions::DEVICE_COHERENT.bits(),
            )
            .expect_err("unaligned HDM base should be rejected");

        assert_eq!(
            err,
            CedtHostBridgeError::InvalidHdmBaseAlignment {
                base: 0x4000_1000,
                alignment: CXL_HPA_ALIGNMENT,
            }
        );
    }

    #[test]
    fn add_cxl_host_bridge_rejects_unaligned_hdm_size() {
        let mut cedt = Cedt::new();
        let err = cedt
            .add_cxl_host_bridge(
                0,
                // Base is 256MiB-aligned, but size is not.
                MemoryRange::new(0x4000_0000..0x5000_1000),
                MemoryRange::new(0x1000_0000..0x1001_0000),
                cxl_spec::spec::CfmwsWindowRestrictions::DEVICE_COHERENT.bits(),
            )
            .expect_err("unaligned HDM size should be rejected");

        assert_eq!(
            err,
            CedtHostBridgeError::InvalidHdmSizeAlignment {
                size: 0x1000_1000,
                alignment: CXL_HPA_ALIGNMENT,
            }
        );
    }
}
