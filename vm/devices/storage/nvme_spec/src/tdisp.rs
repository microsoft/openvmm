// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe TDISP DEVICE_INTERFACE_REPORT definitions.
//!
//! NVMe over PCIe Transport Specification, Revision 1.4, Section 3.8.9,
//! Figure 69, pages 38-39.
//!
//! The containing device-specific format determines where this structure
//! appears within DEVICE_SPECIFIC_INFO.

use bitfield_struct::bitfield;
use inspect::Inspect;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// TDISP NVMe Identifier.
pub const TNI_NVME: u32 = 0x4e56_4d65;

/// Version defined for the NVMe reporting structure.
pub const NVME_TDISP_INFO_VERSION: u8 = 1;

/// TDISP Information (TI), bytes 07:04.
#[derive(Inspect)]
#[bitfield(u32)]
pub struct TdispInformation {
    /// Reporting structure version.
    pub ver: u8,
    /// No admin commands processed since reset, with admin queue head and
    /// tail pointers cleared.
    pub acps: bool,
    /// Value of CC.EN.
    pub env: bool,
    /// Value of CC.SHN.
    #[bits(2)]
    pub shnv: u8,
    /// Value of CSTS.SHST.
    #[bits(2)]
    pub shstv: u8,
    /// Value of CSTS.ST.
    pub stv: bool,
    #[bits(1)]
    _reserved0: u8,
    /// Value of CC.CSS.
    #[bits(3)]
    pub cssv: u8,
    #[bits(1)]
    _reserved1: u8,
    /// Value of CC.MPS.
    #[bits(4)]
    pub mpsv: u8,
    /// Value of CC.IOSQES.
    #[bits(4)]
    pub iosqesv: u8,
    /// Value of CC.IOCQES.
    #[bits(4)]
    pub iocqesv: u8,
}

/// NVMe TDISP DEVICE_INTERFACE_REPORT reporting structure.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect, PartialEq, Eq,
)]
pub struct NvmeTdispInfo {
    /// TDISP NVMe Identifier, bytes 03:00.
    pub tni: u32,
    /// TDISP Information, bytes 07:04.
    pub ti: u32,
    /// Controller State Version Index used for the queue hash, byte 08.
    pub cu: u8,
    /// Controller State structure version used for the queue hash, byte 09.
    pub vu: u8,
    /// Reserved, bytes 11:10.
    #[inspect(skip)]
    pub reserved0: [u8; 2],
    /// AQA Value, bytes 15:12.
    pub aqa: u32,
    /// ASQ Value, bytes 19:16.
    pub asq: u32,
    /// ACQ Value, bytes 23:20.
    pub acq: u32,
    /// CMBMSC Value, bytes 27:24.
    pub cmbmsc: u32,
    /// PMRMSC Value, bytes 31:28.
    pub pmrmsc: u32,
    /// PMRCTL Value, bytes 35:32.
    pub pmrctl: u32,
    /// SHA-384 over the concatenated NVMe Controller State fields,
    /// bytes 83:36.
    #[inspect(iter_by_index)]
    pub queue_hash: [u8; 48],
    /// Reserved, bytes 127:84.
    #[inspect(skip)]
    pub reserved1: [u8; 44],
}

static_assertions::assert_eq_size!(NvmeTdispInfo, [u8; 128]);

impl NvmeTdispInfo {
    /// Decodes the reported TI bitfield.
    pub fn tdisp_information(&self) -> TdispInformation {
        TdispInformation::from(self.ti)
    }
}
