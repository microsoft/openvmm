// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IVRS (I/O Virtualization Reporting Structure) types for AMD IOMMU discovery.
//!
//! The IVRS ACPI table describes AMD IOMMU hardware to the guest OS. It contains
//! one or more IVHD (I/O Virtualization Hardware Definition) blocks, each
//! describing a single IOMMU instance: its PCI BDF, MMIO base, capabilities,
//! and the set of devices behind it.
//!
//! Reference: AMD I/O Virtualization Technology (IOMMU) Specification,
//! Doc #48882, Rev 3.11, §5.

use super::Table;
use crate::packed_nums::*;
use bitfield_struct::bitfield;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

/// IVRS table revision (AMD IOMMU spec §5.2.1).
pub const IVRS_REVISION: u8 = 2;

/// IVHD type 10h: Full-featured IVHD (§5.2.2.2).
pub const IVHD_TYPE_10: u8 = 0x10;

/// IVHD type 11h: Extended IVHD with EFR (§5.2.2.3).
pub const IVHD_TYPE_11: u8 = 0x11;

/// IVHD type 40h: Maximum performance IVHD with EFR (same layout as 11h).
pub const IVHD_TYPE_40: u8 = 0x40;

/// IVHD device entry type: all devices (§5.2.2.7, Table 93).
pub const IVHD_DEV_ALL: u8 = 0x01;

/// IVHD device entry type: select single device (§5.2.2.7, Table 93).
pub const IVHD_DEV_SELECT: u8 = 0x02;

/// IVHD device entry type: start of device range (§5.2.2.7, Table 93).
pub const IVHD_DEV_RANGE_START: u8 = 0x03;

/// IVHD device entry type: end of device range (§5.2.2.7, Table 93).
pub const IVHD_DEV_RANGE_END: u8 = 0x04;

/// IVHD device entry type: special device (§5.2.2.8, Table 96).
///
/// 8-byte entry used for IOAPIC, HPET, and other special devices that
/// are not PCI devices but need IOMMU interrupt remapping.
pub const IVHD_DEV_SPECIAL: u8 = 0x48;

/// IVHD special device variety: IOAPIC (§5.2.2.8, Table 96).
pub const IVHD_SPECIAL_IOAPIC: u8 = 0x01;

/// IVHD special device variety: HPET (§5.2.2.8, Table 96).
pub const IVHD_SPECIAL_HPET: u8 = 0x02;

/// DTE setting: INITPass, EIntPass, NMIPass for fixed interrupt passthrough.
pub const IVHD_DTE_SETTING_INIT_PASS: u8 = 0x01;
pub const IVHD_DTE_SETTING_EINT_PASS: u8 = 0x02;
pub const IVHD_DTE_SETTING_NMI_PASS: u8 = 0x04;

/// IVinfo bitfield for the IVRS table header (AMD IOMMU spec §5.2.1, Table 84).
#[bitfield(u32)]
pub struct IvInfo {
    /// EFRSup: Extended Feature Register supported (bit 0).
    pub efr_sup: bool,
    /// DMA remap support (bit 1).
    pub dma_remap_sup: bool,
    /// Reserved (bits 4:2).
    #[bits(3)]
    _reserved1: u32,
    /// GVAsize: guest virtual address size (bits 7:5).
    /// 0 = 48-bit, 1 = 57-bit.
    #[bits(3)]
    pub gva_size: u8,
    /// PAsize: physical/guest-physical address size (bits 14:8).
    /// Raw value, e.g. 48 for 48-bit.
    #[bits(7)]
    pub pa_size: u8,
    /// VAsize: virtual address size (bits 21:15).
    /// Raw value, e.g. 48 for 48-bit.
    #[bits(7)]
    pub va_size: u8,
    /// HtAtsResv: HyperTransport ATS reserved (bit 22).
    pub ht_ats_resv: bool,
    /// Reserved (bits 31:23).
    #[bits(9)]
    _reserved2: u32,
}

/// IVRS fixed table header (follows the standard ACPI `Header`).
///
/// The IVRS table starts with the standard 36-byte ACPI header, followed by
/// this 12-byte structure, followed by one or more IVHD/IVMD blocks.
///
/// Reference: AMD IOMMU spec §5.2.1, Table 83.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct Ivrs {
    /// IVinfo field (§5.2.1, Table 84). See [`IvInfo`] for the bitfield layout.
    pub iv_info: u32_ne,
    /// Reserved, must be zero.
    pub reserved: [u8; 8],
}

impl Ivrs {
    /// Create a new IVRS header with the given IVinfo value.
    pub fn new(iv_info: u32) -> Self {
        Self {
            iv_info: iv_info.into(),
            reserved: [0; 8],
        }
    }
}

impl Table for Ivrs {
    const SIGNATURE: [u8; 4] = *b"IVRS";
}

const_assert_eq!(size_of::<Ivrs>(), 12);

/// IVHD type 11h header (§5.2.2.3, Table 99).
///
/// Extended IVHD block with EFR image. Types 10h, 11h, and 40h all share
/// the same first 24 bytes; types 11h and 40h extend this with EFR/EFR2
/// register images (bytes 24..40) and are byte-identical apart from the
/// type field. Type 40h additionally permits variable-length (ACPI
/// HID-based) device entries, whereas 11h is limited to fixed-length
/// (BDF-based) entries. Both require `IVinfo[EFRSup] = 1`.
///
/// We emit type 11h rather than 40h because it is the most broadly
/// compatible extended format: the Microsoft hypervisor's boot-time IVRS
/// parser (as shipped in Windows Server 2022 / build 20348) only
/// recognizes types 10h and 11h and rejects the whole IVRS as a bad ACPI
/// table if it finds no matching IVHD, whereas type 40h support is newer.
/// Type 11h is accepted by that parser, newer hypervisor builds, and
/// Linux. Our device entries are all BDF-based, so 40h buys us nothing.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IvhdType11 {
    /// IVHD type: always [`IVHD_TYPE_11`].
    pub ivhd_type: u8,
    /// Flags (§5.2.2.2, Table 87).
    pub flags: u8,
    /// Length of the entire IVHD block in bytes (header + device entries).
    pub length: u16_ne,
    /// DeviceID (BDF) of the IOMMU itself.
    pub device_id: u16_ne,
    /// Offset of the IOMMU capability block in PCI config space.
    pub capability_offset: u16_ne,
    /// IOMMU MMIO base address (64-bit).
    pub iommu_base_address: u64_ne,
    /// PCI segment group number.
    pub pci_segment: u16_ne,
    /// IOMMU info field (§5.2.2.2).
    pub iommu_info: u16_ne,
    /// IOMMU attributes (§5.2.2.3). Repurposed from the type 10h
    /// `iommu_feature_info` field.
    pub iommu_attributes: u32_ne,
    /// Extended Feature Register image (same layout as MMIO 0x0030).
    pub efr_register: u64_ne,
    /// Extended Feature Register 2 image.
    pub efr_register2: u64_ne,
}

impl IvhdType11 {
    /// Create a new IVHD type 11h header.
    pub fn new(
        device_id: u16,
        capability_offset: u16,
        iommu_base_address: u64,
        pci_segment: u16,
        efr: u64,
    ) -> Self {
        Self {
            ivhd_type: IVHD_TYPE_11,
            flags: 0,
            length: (size_of::<Self>() as u16).into(),
            device_id: device_id.into(),
            capability_offset: capability_offset.into(),
            iommu_base_address: iommu_base_address.into(),
            pci_segment: pci_segment.into(),
            iommu_info: 0.into(),
            iommu_attributes: 0.into(),
            efr_register: efr.into(),
            efr_register2: 0.into(),
        }
    }

    /// Set the total length (header + device entries).
    pub fn with_length(mut self, length: u16) -> Self {
        self.length = length.into();
        self
    }

    /// Set the flags byte.
    pub fn with_flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }
}

const_assert_eq!(size_of::<IvhdType11>(), 40);

/// IVHD 4-byte device entry (§5.2.2.7, Table 93).
///
/// Used for: all-devices (type 01h), select (type 02h), start-of-range
/// (type 03h), end-of-range (type 04h).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IvhdDeviceEntry4 {
    /// Entry type (see IVHD_DEV_* constants).
    pub entry_type: u8,
    /// DeviceID (BDF) for select/range entries. Zero for type 01h (all).
    pub device_id: u16_ne,
    /// DTE setting applied to matching devices (§5.2.2.7, Table 94):
    /// - Bit 0: INITPass
    /// - Bit 1: EIntPass
    /// - Bit 2: NMIPass
    /// - Bit 3: Reserved
    /// - Bits 5:4: SysMgt
    /// - Bit 6: Lint0Pass
    /// - Bit 7: Lint1Pass
    pub dte_setting: u8,
}

impl IvhdDeviceEntry4 {
    /// Create an "all devices" entry (type 01h).
    pub fn all(dte_setting: u8) -> Self {
        Self {
            entry_type: IVHD_DEV_ALL,
            device_id: 0.into(),
            dte_setting,
        }
    }

    /// Create a "select" entry (type 02h) for a single device.
    pub fn select(device_id: u16, dte_setting: u8) -> Self {
        Self {
            entry_type: IVHD_DEV_SELECT,
            device_id: device_id.into(),
            dte_setting,
        }
    }

    /// Create a "start of range" entry (type 03h).
    pub fn range_start(device_id: u16, dte_setting: u8) -> Self {
        Self {
            entry_type: IVHD_DEV_RANGE_START,
            device_id: device_id.into(),
            dte_setting,
        }
    }

    /// Create an "end of range" entry (type 04h).
    pub fn range_end(device_id: u16) -> Self {
        Self {
            entry_type: IVHD_DEV_RANGE_END,
            device_id: device_id.into(),
            dte_setting: 0,
        }
    }
}

const_assert_eq!(size_of::<IvhdDeviceEntry4>(), 4);

/// IVHD 8-byte special device entry (§5.2.2.8, Table 96).
///
/// Used for non-PCI devices (IOAPIC, HPET) that need IOMMU interrupt
/// remapping. The `handle` identifies the device instance (e.g., IOAPIC ID
/// or HPET number), and the source RID identifies the remapping context used
/// for DTE/IRTE lookup.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IvhdSpecialDeviceEntry8 {
    /// Entry type (0x48 for special device).
    pub entry_type: u8,
    /// Reserved, must be zero (bytes 1-2 per spec Table 107).
    _reserved: u16_ne,
    /// DTE setting (same as `IvhdDeviceEntry4::dte_setting`).
    pub dte_setting: u8,
    /// Device handle — interpretation depends on `variety`:
    /// - IOAPIC: IOAPIC ID (from ACPI MADT)
    /// - HPET: HPET number
    pub handle: u8,
    /// Source DeviceID — the BDF used by the IOMMU for DTE/IRTE lookup
    /// when this special device issues an interrupt.
    pub source_device_id: u16_ne,
    /// Variety of special device (see `IVHD_SPECIAL_*` constants).
    pub variety: u8,
}

impl IvhdSpecialDeviceEntry8 {
    /// Create a special device entry for an IOAPIC.
    ///
    /// - `rid`: IOAPIC RID for IOMMU DTE/IRTE lookup
    /// - `ioapic_id`: the IOAPIC ID (matches the MADT entry)
    pub fn ioapic(rid: u16, ioapic_id: u8) -> Self {
        Self {
            entry_type: IVHD_DEV_SPECIAL,
            _reserved: 0.into(),
            dte_setting: 0,
            handle: ioapic_id,
            source_device_id: rid.into(),
            variety: IVHD_SPECIAL_IOAPIC,
        }
    }
}

const_assert_eq!(size_of::<IvhdSpecialDeviceEntry8>(), 8);

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec::Vec;
    use zerocopy::IntoBytes;

    #[test]
    fn test_ivrs_header() {
        let ivrs = Ivrs::new(0x0040_3000);
        assert_eq!(ivrs.iv_info.get(), 0x0040_3000);
        assert_eq!(ivrs.reserved, [0; 8]);
        let bytes = ivrs.as_bytes();
        assert_eq!(bytes.len(), 12);
    }

    #[test]
    fn test_iv_info_bitfield() {
        let iv_info = IvInfo::new()
            .with_efr_sup(true)
            .with_pa_size(48)
            .with_va_size(48);
        let raw = u32::from(iv_info);
        assert_eq!(raw & 1, 1); // EFRSup = bit 0
        assert_eq!((raw >> 8) & 0x7F, 48); // PAsize = bits 14:8
        assert_eq!((raw >> 15) & 0x7F, 48); // VAsize = bits 21:15
    }

    #[test]
    fn test_ivrs_signature() {
        assert_eq!(Ivrs::SIGNATURE, *b"IVRS");
    }

    #[test]
    fn test_ivhd_type11_defaults() {
        let ivhd = IvhdType11::new(0x0002, 0x40, 0xFD00_0000, 0, 0xC0);
        assert_eq!(ivhd.ivhd_type, IVHD_TYPE_11);
        assert_eq!(ivhd.flags, 0);
        assert_eq!(ivhd.length.get(), size_of::<IvhdType11>() as u16);
        assert_eq!(ivhd.device_id.get(), 0x0002);
        assert_eq!(ivhd.capability_offset.get(), 0x40);
        assert_eq!(ivhd.iommu_base_address.get(), 0xFD00_0000);
        assert_eq!(ivhd.pci_segment.get(), 0);
        assert_eq!(ivhd.iommu_info.get(), 0);
        assert_eq!(ivhd.iommu_attributes.get(), 0);
        assert_eq!(ivhd.efr_register.get(), 0xC0);
        assert_eq!(ivhd.efr_register2.get(), 0);
    }

    #[test]
    fn test_ivhd_type11_with_length() {
        let ivhd = IvhdType11::new(0x0002, 0x40, 0xFD00_0000, 0, 0).with_length(48);
        assert_eq!(ivhd.length.get(), 48);
    }

    #[test]
    fn test_ivhd_device_entry_all() {
        let entry = IvhdDeviceEntry4::all(0);
        assert_eq!(entry.entry_type, IVHD_DEV_ALL);
        assert_eq!(entry.device_id.get(), 0);
        assert_eq!(entry.dte_setting, 0);
    }

    #[test]
    fn test_ivhd_device_entry_select() {
        let entry = IvhdDeviceEntry4::select(0x0108, 0x07);
        assert_eq!(entry.entry_type, IVHD_DEV_SELECT);
        assert_eq!(entry.device_id.get(), 0x0108);
        assert_eq!(entry.dte_setting, 0x07);
    }

    #[test]
    fn test_ivhd_device_entry_range() {
        let start = IvhdDeviceEntry4::range_start(0x0001, 0);
        let end = IvhdDeviceEntry4::range_end(0xFFFF);
        assert_eq!(start.entry_type, IVHD_DEV_RANGE_START);
        assert_eq!(start.device_id.get(), 0x0001);
        assert_eq!(end.entry_type, IVHD_DEV_RANGE_END);
        assert_eq!(end.device_id.get(), 0xFFFF);
    }

    #[test]
    fn test_ivhd_device_entry_size() {
        assert_eq!(size_of::<IvhdDeviceEntry4>(), 4);
    }

    #[test]
    fn test_ivrs_round_trip() {
        // Build a minimal IVRS: header + IVHD + two device entries
        let ivrs = Ivrs::new(0x0040_3000);
        let dev_entries_size = 2 * size_of::<IvhdDeviceEntry4>() as u16;
        let ivhd_total = size_of::<IvhdType11>() as u16 + dev_entries_size;
        let ivhd = IvhdType11::new(0x0002, 0x40, 0xFD00_0000, 0, 0).with_length(ivhd_total);

        let range_start = IvhdDeviceEntry4::range_start(0x0001, 0);
        let range_end = IvhdDeviceEntry4::range_end(0xFFFF);

        // Serialize and verify offsets
        let mut buf = Vec::new();
        buf.extend_from_slice(ivrs.as_bytes());
        buf.extend_from_slice(ivhd.as_bytes());
        buf.extend_from_slice(range_start.as_bytes());
        buf.extend_from_slice(range_end.as_bytes());

        // IVRS header = 12 bytes
        // IVHD header = 40 bytes
        // 2 device entries = 8 bytes
        // Total = 60 bytes
        assert_eq!(buf.len(), 60);

        // Verify IVHD starts at offset 12
        assert_eq!(buf[12], IVHD_TYPE_11);
        // Verify device entries start at offset 52 (12 + 40)
        assert_eq!(buf[52], IVHD_DEV_RANGE_START);
        assert_eq!(buf[56], IVHD_DEV_RANGE_END);
    }
}
