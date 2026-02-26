// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hardware definitions for the GDMA/MANA device, which is the NIC exposed by
//! new Azure hardware SKUs.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

pub mod access;
pub mod bnic;

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use std::fmt::Debug;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const VENDOR_ID: u16 = 0x1414;
pub const DEVICE_ID: u16 = 0x00BA;

pub const PAGE_SIZE32: u32 = 4096;
pub const PAGE_SIZE64: u64 = 4096;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct RegMap {
    #[inspect(hex)]
    pub micro_version_number: u16,
    #[inspect(hex)]
    pub minor_version_number: u8,
    #[inspect(hex)]
    pub major_version_number: u8,
    #[inspect(hex)]
    pub reserved: u32,
    #[inspect(hex)]
    pub vf_db_pages_zone_offset: u64,
    #[inspect(hex)]
    pub vf_db_page_sz: u16,
    #[inspect(hex)]
    pub reserved2: u16,
    #[inspect(hex)]
    pub reserved3: u32,
    #[inspect(hex)]
    pub vf_gdma_sriov_shared_reg_start: u64,
    #[inspect(hex)]
    pub vf_gdma_sriov_shared_sz: u16,
    #[inspect(hex)]
    pub reserved4: u16,
    #[inspect(hex)]
    pub reserved5: u32,
}

pub const DB_SQ: u32 = 0;
pub const DB_RQ: u32 = 0x400;
pub const DB_RQ_CLIENT_DATA: u32 = 0x408;
pub const DB_CQ: u32 = 0x800;
pub const DB_EQ: u32 = 0xff8;

#[bitfield(u64)]
pub struct CqEqDoorbellValue {
    #[bits(24)]
    pub id: u32,
    pub reserved: u8,
    #[bits(31)]
    pub tail: u32,
    pub arm: bool,
}

#[bitfield(u64)]
pub struct WqDoorbellValue {
    #[bits(24)]
    pub id: u32,
    pub num_rwqe: u8,
    pub tail: u32,
}

// Shmem
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SmcProtoHdr {
    #[bits(3)]
    pub msg_type: u8,
    #[bits(3)]
    pub msg_version: u8,
    pub reserved_1: bool,
    pub is_response: bool,
    pub status: u8,
    pub reserved_2: u8,
    pub reset_vf: bool,
    #[bits(6)]
    pub reserved_3: u8,
    pub owner_is_pf: bool,
}

open_enum! {
    pub enum SmcMessageType: u8 {
        SMC_MSG_TYPE_ESTABLISH_HWC = 1,
        SMC_MSG_TYPE_DESTROY_HWC = 2,
        SMC_MSG_TYPE_REPORT_HWC_TIMEOUT = 4,
    }
}

pub const SMC_MSG_TYPE_ESTABLISH_HWC_VERSION: u8 = 0;
pub const SMC_MSG_TYPE_DESTROY_HWC_VERSION: u8 = 0;
pub const SMC_MSG_TYPE_REPORT_HWC_TIMEOUT_VERSION: u8 = 1;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EstablishHwc {
    pub eq: [u8; 6],
    pub cq: [u8; 6],
    pub rq: [u8; 6],
    pub sq: [u8; 6],
    pub high: u16,
    pub msix: u16,
    pub hdr: SmcProtoHdr,
}

// Wq
#[repr(C, align(8))]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Wqe {
    pub header: WqeHeader,
    pub data: [u8; 512 - 8],
}

pub const WQE_ALIGNMENT: usize = 32;

impl Wqe {
    pub fn oob(&self) -> &[u8] {
        &self.data[..self.header.oob_len()]
    }

    pub fn sgl(&self) -> &[Sge] {
        <[Sge]>::ref_from_prefix_with_elems(
            &self.data[self.header.sgl_offset()..],
            self.header.params.num_sgl_entries() as usize,
        )
        .unwrap()
        .0
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct WqeHeader {
    pub reserved: [u8; 3],
    pub last_vbytes: u8,
    pub params: WqeParams,
}

impl WqeHeader {
    pub fn total_len(&self) -> usize {
        (self.data_len() + 8 + WQE_ALIGNMENT - 1) & !(WQE_ALIGNMENT - 1)
    }

    pub fn data_len(&self) -> usize {
        self.oob_len() + self.sgl_len()
    }

    pub fn oob_len(&self) -> usize {
        match self.params.inline_client_oob_size() {
            CLIENT_OOB_8 => 8,
            CLIENT_OOB_24 => 24,
            CLIENT_OOB_32 => 32,
            _ => 8,
        }
    }

    pub fn sgl_offset(&self) -> usize {
        ((8 + self.oob_len() + 15) & !15) - 8
    }

    pub fn sgl_len(&self) -> usize {
        self.params.num_sgl_entries() as usize * 16
    }

    pub fn sgl_direct_len(&self) -> usize {
        debug_assert!(self.params.sgl_direct());
        let last = (self.last_vbytes.wrapping_sub(1) & 15) + 1;
        self.sgl_len().wrapping_sub(16).wrapping_add(last as usize) & 31
    }
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct WqeParams {
    pub num_sgl_entries: u8,
    #[bits(3)]
    pub inline_client_oob_size: u8,
    pub client_oob_in_sgl: bool,
    #[bits(4)]
    pub reserved: u8,
    #[bits(14)]
    pub gd_client_unit_data: u16,
    pub reserved2: bool,
    pub sgl_direct: bool,
}

pub const CLIENT_OOB_8: u8 = 2;
pub const CLIENT_OOB_24: u8 = 6;
pub const CLIENT_OOB_32: u8 = 7;

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Sge {
    pub address: u64,
    pub mem_key: u32,
    pub size: u32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cqe {
    pub data: [u8; 60],
    pub params: CqeParams,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CqeParams {
    #[bits(24)]
    pub wq_number: u32,
    pub is_send_wq: bool,
    pub cmpln: bool,
    #[bits(3)]
    pub reserved: u8,
    #[bits(3)]
    pub owner_count: u8,
}

pub const OWNER_BITS: u32 = 3;
pub const OWNER_MASK: u32 = (1 << OWNER_BITS) - 1;

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Eqe {
    pub data: [u8; 12],
    pub params: EqeParams,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EqeParams {
    pub event_type: u8,
    pub reserved: u8,
    #[bits(13)]
    pub reserved2: u16,
    #[bits(3)]
    pub owner_count: u8,
}

pub const GDMA_EQE_COMPLETION: u8 = 3;
pub const GDMA_EQE_TEST_EVENT: u8 = 64;
pub const GDMA_EQE_HWC_INIT_EQ_ID_DB: u8 = 129;
pub const GDMA_EQE_HWC_INIT_DATA: u8 = 130;
pub const GDMA_EQE_HWC_INIT_DONE: u8 = 131;
pub const GDMA_EQE_HWC_RECONFIG_DATA: u8 = 133;
// Sent on the event of a SoC Crash or certain Firmware updates.
pub const GDMA_EQE_HWC_RECONFIG_VF: u8 = 135;

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcInitEqIdDb {
    pub eq_id: u16,
    pub doorbell: u16,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcInitTypeData {
    #[bits(24)]
    pub value: u32,
    pub ty: u8,
}

pub const HWC_DATA_CONFIG_HWC_TIMEOUT: u8 = 1;
pub const HWC_DATA_TYPE_HW_VPORT_LINK_CONNECT: u8 = 2;
pub const HWC_DATA_TYPE_HW_VPORT_LINK_DISCONNECT: u8 = 3;
#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EqeDataReconfig {
    pub data: [u8; 3],
    pub data_type: u8,
    pub reserved1: [u8; 8],
}

pub const HWC_INIT_DATA_CQID: u8 = 1;
pub const HWC_INIT_DATA_RQID: u8 = 2;
pub const HWC_INIT_DATA_SQID: u8 = 3;
pub const HWC_INIT_DATA_QUEUE_DEPTH: u8 = 4;
pub const HWC_INIT_DATA_MAX_REQUEST: u8 = 5;
pub const HWC_INIT_DATA_MAX_RESPONSE: u8 = 6;
pub const HWC_INIT_DATA_MAX_NUM_CQS: u8 = 7;
pub const HWC_INIT_DATA_PDID: u8 = 8;
pub const HWC_INIT_DATA_GPA_MKEY: u8 = 9;

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum GdmaRequestType : u32 {
        GDMA_VERIFY_VF_DRIVER_VERSION = 1,
        GDMA_QUERY_MAX_RESOURCES = 2,
        GDMA_LIST_DEVICES = 3,
        GDMA_REGISTER_DEVICE = 4,
        GDMA_DEREGISTER_DEVICE = 5,
        GDMA_GENERATE_TEST_EQE = 10,
        GDMA_GENERATE_RECONFIG_VF_EVENT = 11,
        GDMA_CREATE_QUEUE = 12,
        GDMA_DISABLE_QUEUE = 13,
        GDMA_CREATE_DMA_REGION = 25,
        GDMA_DMA_REGION_ADD_PAGES = 26,
        GDMA_DESTROY_DMA_REGION = 27,
        GDMA_CHANGE_MSIX_FOR_EQ = 81,
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaMsgHdr {
    pub hdr_type: u32,
    pub msg_type: u32,
    pub msg_version: u16,
    pub hwc_msg_id: u16,
    pub msg_size: u32,
}

pub const GDMA_STANDARD_HEADER_TYPE: u32 = 0;

pub const GDMA_MESSAGE_V1: u16 = 1;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct GdmaDevId {
    pub ty: GdmaDevType,
    pub instance: u16,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum GdmaDevType: u16 {
        GDMA_DEVICE_NONE = 0,
        GDMA_DEVICE_HWC = 1,
        GDMA_DEVICE_MANA = 2,
    }
}

pub const HWC_DEV_ID: GdmaDevId = GdmaDevId {
    ty: GdmaDevType::GDMA_DEVICE_HWC,
    instance: 0,
};

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaReqHdr {
    pub req: GdmaMsgHdr,
    pub resp: GdmaMsgHdr,
    pub dev_id: GdmaDevId,
    pub activity_id: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaRespHdr {
    pub response: GdmaMsgHdr,
    pub dev_id: GdmaDevId,
    pub activity_id: u32,
    pub status: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaGenerateTestEventReq {
    pub queue_index: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcTxOob {
    pub reserved: u64,
    pub flags1: HwcTxOobFlags1,
    pub flags2: HwcTxOobFlags2,
    pub flags3: HwcTxOobFlags3,
    pub flags4: HwcTxOobFlags4,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcTxOobFlags1 {
    #[bits(24)]
    pub vrq_id: u32,
    pub dest_vfid: u8,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcTxOobFlags2 {
    #[bits(24)]
    pub vrcq_id: u32,
    pub reserved: u8,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcTxOobFlags3 {
    #[bits(24)]
    pub vscq_id: u32,
    pub loopback: bool,
    pub lso_override: bool,
    pub dest_pf: bool,
    #[bits(5)]
    pub reserved: u8,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcTxOobFlags4 {
    #[bits(24)]
    pub vsq_id: u32,
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcRxOob {
    pub flags: HwcRxOobFlags,
    pub reserved2: u32,
    pub wqe_addr_low_or_offset: u32,
    pub wqe_addr_high: u32,
    pub client_data_unit: u32,
    pub tx_oob_data_size: u32,
    pub chunk_offset: u32,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HwcRxOobFlags {
    #[bits(6)]
    pub ty: u8,
    pub eom: bool,
    pub som: bool,
    pub vendor_err: u8,
    pub reserved1: u16,

    #[bits(24)]
    pub src_virt_wq: u32,
    pub src_vfid: u8,
}

pub const DRIVER_CAP_FLAG_1_HWC_TIMEOUT_RECONFIG: u64 = 0x08;
pub const DRIVER_CAP_FLAG_1_VARIABLE_INDIRECTION_TABLE_SUPPORT: u64 = 0x20;
pub const DRIVER_CAP_FLAG_1_HW_VPORT_LINK_AWARE: u64 = 0x40;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaVerifyVerReq {
    pub protocol_ver_min: u64,
    pub protocol_ver_max: u64,
    pub gd_drv_cap_flags1: u64,
    pub gd_drv_cap_flags2: u64,
    pub gd_drv_cap_flags3: u64,
    pub gd_drv_cap_flags4: u64,
    pub drv_ver: u64,
    pub os_type: u32,
    pub reserved: u32,
    pub os_ver_major: u32,
    pub os_ver_minor: u32,
    pub os_ver_build: u32,
    pub os_ver_platform: u32,
    pub reserved_2: u64,
    pub os_ver_str1: [u8; 128],
    pub os_ver_str2: [u8; 128],
    pub os_ver_str3: [u8; 128],
    pub os_ver_str4: [u8; 128],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaVerifyVerResp {
    pub gdma_protocol_ver: u64,
    pub pf_cap_flags1: u64,
    pub pf_cap_flags2: u64,
    pub pf_cap_flags3: u64,
    pub pf_cap_flags4: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaQueryMaxResourcesResp {
    pub status: u32,
    pub max_sq: u32,
    pub max_rq: u32,
    pub max_cq: u32,
    pub max_eq: u32,
    pub max_db: u32,
    pub max_mst: u32,
    pub max_cq_mod_ctx: u32,
    pub max_mod_cq: u32,
    pub max_msix: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaListDevicesResp {
    pub num_of_devs: u32,
    pub reserved: u32,
    pub devs: [GdmaDevId; 64],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaRegisterDeviceResp {
    pub pdid: u32,
    pub gpa_mkey: u32,
    pub db_id: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaCreateDmaRegionReq {
    pub length: u64,
    pub offset_in_page: u32,
    pub gdma_page_type: u32,
    pub page_count: u32,
    pub page_addr_list_len: u32,
    // Followed by u64 page list.
}

pub const GDMA_PAGE_TYPE_4K: u32 = 0;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaCreateDmaRegionResp {
    pub gdma_region: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaDestroyDmaRegionReq {
    pub gdma_region: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaCreateQueueReq {
    pub queue_type: GdmaQueueType,
    pub reserved1: u32,
    pub pdid: u32,
    pub doorbell_id: u32,
    pub gdma_region: u64,
    pub reserved2: u32,
    pub queue_size: u32,
    pub log2_throttle_limit: u32,
    pub eq_pci_msix_index: u32,
    pub cq_mod_ctx_id: u32,
    pub cq_parent_eq_id: u32,
    pub rq_drop_on_overrun: u8,
    pub rq_err_on_wqe_overflow: u8,
    pub rq_chain_rec_wqes: u8,
    pub sq_hw_db: u8,
    pub reserved3: u32,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum GdmaQueueType: u32 {
        GDMA_SQ = 1,
        GDMA_RQ = 2,
        GDMA_CQ = 3,
        GDMA_EQ = 4,
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaCreateQueueResp {
    pub queue_index: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaDisableQueueReq {
    pub queue_type: GdmaQueueType,
    pub queue_index: u32,
    pub alloc_res_id_on_creation: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdmaChangeMsixVectorIndexForEq {
    pub queue_index: u32,
    pub msix: u32,
    pub reserved1: u32,
    pub reserved2: u32,
}

/// Test hook constants used by the GDMA emulator and net_mana tests.
/// These are only available when the `test_hooks` feature is enabled.
#[cfg(feature = "test_hooks")]
pub mod test_hooks {
    /// When a packet with this length is received, the GDMA BNIC emulator
    /// will return CQE_RX_ERR_DISABLED_QUEUE to simulate an RX error.
    ///
    /// The value (0xDEAD = 57,005 bytes) is chosen to be well above the maximum
    /// jumbo frame size (MAX_MTU = 9,216 bytes), so that it does not collide with any
    /// legitimate network packet, while remaining small enough to allocate
    /// in test guest memory (2MB).
    pub const RX_ERROR_TRIGGER_PACKET_LEN: usize = 0xDEAD;
}
