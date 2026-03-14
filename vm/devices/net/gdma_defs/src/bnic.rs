// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GDMA Basic NIC (BNIC/MANA) definitions

use super::GdmaQueueType;
use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum! {
    pub enum ManaCommandCode: u32 {
        MANA_QUERY_DEV_CONFIG = 0x20001,
        MANA_QUERY_STATS = 0x20002,
        MANA_CONFIG_VPORT_TX = 0x20003,
        MANA_CREATE_WQ_OBJ = 0x20004,
        MANA_DESTROY_WQ_OBJ = 0x20005,
        MANA_FENCE_RQ = 0x20006,
        MANA_CONFIG_VPORT_RX = 0x20007,
        MANA_QUERY_VPORT_CONFIG = 0x20008,
        MANA_VTL2_ASSIGN_SERIAL_NUMBER = 0x27801,
        MANA_VTL2_MOVE_FILTER = 0x27802,
        MANA_VTL2_QUERY_FILTER_STATE = 0x27803,
    }
}

pub const MANA_VTL2_MOVE_FILTER_REQUEST_V2: u16 = 2;
pub const MANA_VTL2_MOVE_FILTER_RESPONSE_V1: u16 = 1;
pub const MANA_VTL2_ASSIGN_SERIAL_NUMBER_REQUEST_V1: u16 = 1;
pub const MANA_VTL2_ASSIGN_SERIAL_NUMBER_RESPONSE_V1: u16 = 1;
pub const MANA_VTL2_QUERY_FILTER_STATE_REQUEST_V1: u16 = 1;
pub const MANA_VTL2_QUERY_FILTER_STATE_RESPONSE_V1: u16 = 1;

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BasicNicDriverFlags {
    #[bits(1)]
    pub query_link_status: u8,
    #[bits(1)]
    pub ethertype_enforcement: u8,
    #[bits(1)]
    pub query_filter_state: u8,
    #[bits(61)]
    reserved: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryDeviceCfgReq {
    pub mn_drv_cap_flags1: u64,
    pub mn_drv_cap_flags2: u64,
    pub mn_drv_cap_flags3: u64,
    pub mn_drv_cap_flags4: u64,

    pub proto_major_ver: u32,
    pub proto_minor_ver: u32,
    pub proto_micro_ver: u32,

    pub reserved: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryDeviceCfgResp {
    pub pf_cap_flags1: BasicNicDriverFlags,
    pub pf_cap_flags2: u64,
    pub pf_cap_flags3: u64,
    pub pf_cap_flags4: u64,

    pub max_num_vports: u16,
    pub reserved: u16,
    pub max_num_eqs: u32,
}

impl std::fmt::Debug for ManaQueryDeviceCfgResp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManaQueryDeviceCfgResp")
            .field("pf_cap_flags1", &self.pf_cap_flags1)
            .field("pf_cap_flags2", &self.pf_cap_flags2)
            .field("pf_cap_flags3", &self.pf_cap_flags3)
            .field("pf_cap_flags4", &self.pf_cap_flags4)
            .field("max_num_vports", &self.max_num_vports)
            .field("reserved", &self.reserved)
            .field("max_num_eqs", &self.max_num_eqs)
            .finish()
    }
}

impl ManaQueryDeviceCfgResp {
    pub fn cap_query_link_status(&self) -> bool {
        self.pf_cap_flags1.query_link_status() != 0
    }
    pub fn cap_ethertype_enforcement(&self) -> bool {
        self.pf_cap_flags1.ethertype_enforcement() != 0
    }
    pub fn cap_filter_state_query(&self) -> bool {
        self.pf_cap_flags1.query_filter_state() != 0
    }
}

/* Query vPort Configuration */
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryVportCfgReq {
    pub vport_index: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryVportCfgResp {
    pub max_num_sq: u32,
    pub max_num_rq: u32,
    pub num_indirection_ent: u32,
    pub reserved1: u32,
    pub mac_addr: [u8; 6],
    pub reserved2: [u8; 2],
    pub vport: u64,
}

/* Move Filter invoked from VTL2 to move filter from VTL2 to VTL0 and back*/
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaMoveFilterVTL2PrivilegedReq {
    pub vport: u64,
    pub direction_to_vtl0: u8,
    pub reserved: [u8; 3],
    pub reserved2: u32,
}

/* Set vport serial number. */
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaSetVportSerialNo {
    pub vport: u64,
    pub serial_no: u32,
    pub reserved: u32,
}

/* Get vport Filter State. */
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryFilterStateReq {
    pub vport: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryFilterStateResponse {
    pub direction_to_vtl0: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaConfigVportReq {
    pub vport: u64,
    pub pdid: u32,
    pub doorbell_pageid: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaConfigVportResp {
    pub tx_vport_offset: u16,
    pub short_form_allowed: u8,
    pub reserved: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaCreateWqobjReq {
    pub vport: u64,
    pub wq_type: GdmaQueueType,
    pub reserved: u32,
    pub wq_gdma_region: u64,
    pub cq_gdma_region: u64,
    pub wq_size: u32,
    pub cq_size: u32,
    pub cq_moderation_ctx_id: u32,
    pub cq_parent_qid: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaCreateWqobjResp {
    pub wq_id: u32,
    pub cq_id: u32,
    pub wq_obj: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaDestroyWqobjReq {
    pub wq_type: GdmaQueueType,
    pub reserved: u32,
    pub wq_obj_handle: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaFenceRqReq {
    pub wq_obj_handle: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaCfgRxSteerReq {
    pub vport: u64,
    pub num_indir_entries: u16,
    pub indir_tab_offset: u16,
    pub rx_enable: Tristate,
    pub rss_enable: Tristate,
    pub update_default_rxobj: u8,
    pub update_hashkey: u8,
    pub update_indir_tab: u8,
    pub reserved: u8,
    pub default_rxobj: u64,
    pub hashkey: [u8; 40],
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct Tristate(pub u32);

impl Tristate {
    pub const TRUE: Self = Self(1);
    pub const FALSE: Self = Self(0);
    pub const UNKNOWN: Self = Self(!0);
}

impl From<Option<bool>> for Tristate {
    fn from(v: Option<bool>) -> Self {
        match v {
            Some(true) => Self::TRUE,
            Some(false) => Self::FALSE,
            None => Self::UNKNOWN,
        }
    }
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaCqeHeader {
    #[bits(6)]
    pub cqe_type: u8,
    #[bits(2)]
    pub client_type: u8,
    #[bits(24)]
    pub vendor_err: u32,
}

pub const CQE_INVALID: u8 = 0;
pub const CQE_RX_OKAY: u8 = 1;
pub const CQE_RX_COALESCED_4: u8 = 2;
pub const CQE_RX_OBJECT_FENCE: u8 = 3;
pub const CQE_RX_TRUNCATED: u8 = 4;
pub const CQE_RX_ERR_DISABLED_QUEUE: u8 = 5;

pub const CQE_TX_OKAY: u8 = 32;
pub const CQE_TX_SA_DROP: u8 = 33;
pub const CQE_TX_MTU_DROP: u8 = 34;
pub const CQE_TX_INVALID_OOB: u8 = 35;
pub const CQE_TX_INVALID_ETH_TYPE: u8 = 36;
pub const CQE_TX_HDR_PROCESSING_ERROR: u8 = 37;
pub const CQE_TX_VF_DISABLED: u8 = 38;
pub const CQE_TX_VPORT_IDX_OUT_OF_RANGE: u8 = 39;
pub const CQE_TX_VPORT_DISABLED: u8 = 40;
pub const CQE_TX_VLAN_TAGGING_VIOLATION: u8 = 41;
pub const CQE_TX_GDMA_ERR: u8 = 42;

pub const MANA_CQE_COMPLETION: u8 = 1;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaTxCompOob {
    pub cqe_hdr: ManaCqeHeader,
    pub tx_data_offset: u32,
    pub offsets: ManaTxCompOobOffsets,
    pub reserved: [u32; 12],
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaTxCompOobOffsets {
    #[bits(5)]
    pub tx_sgl_offset: u32,
    #[bits(27)]
    pub tx_wqe_offset: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaRxcompPerpktInfo {
    pub pkt_len: u16,
    pub reserved1: u16,
    pub reserved2: u32,
    pub pkt_hash: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaRxcompOob {
    pub cqe_hdr: ManaCqeHeader,
    pub flags: ManaRxcompOobFlags,
    pub ppi: [ManaRxcompPerpktInfo; 4],
    pub rx_wqe_offset: u32,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaRxcompOobFlags {
    #[bits(12)]
    pub rx_vlan_id: u32,
    pub rx_vlantag_present: bool,
    pub rx_outer_iphdr_csum_succeed: bool,
    pub rx_outer_iphdr_csum_fail: bool,
    pub reserved1: bool,
    #[bits(9)]
    pub rx_hashtype: u16,
    pub rx_iphdr_csum_succeed: bool,
    pub rx_iphdr_csum_fail: bool,
    pub rx_tcp_csum_succeed: bool,
    pub rx_tcp_csum_fail: bool,
    pub rx_udp_csum_succeed: bool,
    pub rx_udp_csum_fail: bool,
    pub reserved2: bool,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaTxShortOob {
    #[bits(2)]
    pub pkt_fmt: u8,
    pub is_outer_ipv4: bool,
    pub is_outer_ipv6: bool,
    pub comp_iphdr_csum: bool,
    pub comp_tcp_csum: bool,
    pub comp_udp_csum: bool,
    pub suppress_txcqe_gen: bool,
    #[bits(24)]
    pub vcq_num: u32,

    #[bits(10)]
    pub trans_off: u16,
    #[bits(14)]
    pub vsq_frame: u16,
    pub short_vp_offset: u8,
}

pub const MANA_SHORT_PKT_FMT: u8 = 0;
pub const MANA_LONG_PKT_FMT: u8 = 1;

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaTxLongOob {
    pub is_encap: bool,
    pub inner_is_ipv6: bool,
    pub inner_tcp_opt: bool,
    pub inject_vlan_pri_tag: bool,
    #[bits(12)]
    pub reserved1: u32,
    #[bits(3)]
    pub pcp: u8,
    pub dei: bool,
    #[bits(12)]
    pub vlan_id: u16,

    #[bits(10)]
    pub inner_frame_offset: u16,
    #[bits(6)]
    pub inner_ip_rel_offset: u16,
    #[bits(12)]
    pub long_vp_offset: u16,
    #[bits(4)]
    pub reserved2: u16,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaTxOob {
    pub s_oob: ManaTxShortOob,
    pub l_oob: ManaTxLongOob,
    pub reserved3: u32,
    pub reserved4: u32,
}

pub const STATISTICS_FLAGS_IN_DISCARDS_NO_WQE: u64 = 0x0000000000000001;
pub const STATISTICS_FLAGS_IN_ERRORS_RX_VPORT_DISABLED: u64 = 0x0000000000000002;
pub const STATISTICS_FLAGS_HC_IN_OCTETS: u64 = 0x0000000000000004;
pub const STATISTICS_FLAGS_HC_IN_UCAST_PACKETS: u64 = 0x0000000000000008;
pub const STATISTICS_FLAGS_HC_IN_UCAST_OCTETS: u64 = 0x0000000000000010;
pub const STATISTICS_FLAGS_HC_IN_MCAST_PACKETS: u64 = 0x0000000000000020;
pub const STATISTICS_FLAGS_HC_IN_MCAST_OCTETS: u64 = 0x0000000000000040;
pub const STATISTICS_FLAGS_HC_IN_BCAST_PACKETS: u64 = 0x0000000000000080;
pub const STATISTICS_FLAGS_HC_IN_BCAST_OCTETS: u64 = 0x0000000000000100;
pub const STATISTICS_FLAGS_OUT_ERRORS_GF_DISABLED: u64 = 0x0000000000000200;
pub const STATISTICS_FLAGS_OUT_ERRORS_VPORT_DISABLED: u64 = 0x0000000000000400;
pub const STATISTICS_FLAGS_OUT_ERRORS_INVALID_VPORT_OFFSET_PACKETS: u64 = 0x0000000000000800;
pub const STATISTICS_FLAGS_OUT_ERRORS_VLAN_ENFORCEMENT: u64 = 0x0000000000001000;
pub const STATISTICS_FLAGS_OUT_ERRORS_ETH_TYPE_ENFORCEMENT: u64 = 0x0000000000002000;
pub const STATISTICS_FLAGS_OUT_ERRORS_SA_ENFORCEMENT: u64 = 0x0000000000004000;
pub const STATISTICS_FLAGS_OUT_ERRORS_SQPDID_ENFORCEMENT: u64 = 0x0000000000008000;
pub const STATISTICS_FLAGS_OUT_ERRORS_CQPDID_ENFORCEMENT: u64 = 0x0000000000010000;
pub const STATISTICS_FLAGS_OUT_ERRORS_MTU_VIOLATION: u64 = 0x0000000000020000;
pub const STATISTICS_FLAGS_OUT_ERRORS_INVALID_OOB: u64 = 0x0000000000040000;
pub const STATISTICS_FLAGS_HC_OUT_OCTETS: u64 = 0x0000000000080000;
pub const STATISTICS_FLAGS_HC_OUT_UCAST_PACKETS: u64 = 0x0000000000100000;
pub const STATISTICS_FLAGS_HC_OUT_UCAST_OCTETS: u64 = 0x0000000000200000;
pub const STATISTICS_FLAGS_HC_OUT_MCAST_PACKETS: u64 = 0x0000000000400000;
pub const STATISTICS_FLAGS_HC_OUT_MCAST_OCTETS: u64 = 0x0000000000800000;
pub const STATISTICS_FLAGS_HC_OUT_BCAST_PACKETS: u64 = 0x0000000001000000;
pub const STATISTICS_FLAGS_HC_OUT_BCAST_OCTETS: u64 = 0x0000000002000000;

pub const STATISTICS_FLAGS_ALL: u64 = STATISTICS_FLAGS_IN_DISCARDS_NO_WQE
    | STATISTICS_FLAGS_IN_ERRORS_RX_VPORT_DISABLED
    | STATISTICS_FLAGS_HC_IN_OCTETS
    | STATISTICS_FLAGS_HC_IN_UCAST_PACKETS
    | STATISTICS_FLAGS_HC_IN_UCAST_OCTETS
    | STATISTICS_FLAGS_HC_IN_MCAST_PACKETS
    | STATISTICS_FLAGS_HC_IN_MCAST_OCTETS
    | STATISTICS_FLAGS_HC_IN_BCAST_PACKETS
    | STATISTICS_FLAGS_HC_IN_BCAST_OCTETS
    | STATISTICS_FLAGS_OUT_ERRORS_GF_DISABLED
    | STATISTICS_FLAGS_OUT_ERRORS_VPORT_DISABLED
    | STATISTICS_FLAGS_OUT_ERRORS_INVALID_VPORT_OFFSET_PACKETS
    | STATISTICS_FLAGS_OUT_ERRORS_VLAN_ENFORCEMENT
    | STATISTICS_FLAGS_OUT_ERRORS_ETH_TYPE_ENFORCEMENT
    | STATISTICS_FLAGS_OUT_ERRORS_SA_ENFORCEMENT
    | STATISTICS_FLAGS_OUT_ERRORS_SQPDID_ENFORCEMENT
    | STATISTICS_FLAGS_OUT_ERRORS_CQPDID_ENFORCEMENT
    | STATISTICS_FLAGS_OUT_ERRORS_MTU_VIOLATION
    | STATISTICS_FLAGS_OUT_ERRORS_INVALID_OOB
    | STATISTICS_FLAGS_HC_OUT_OCTETS
    | STATISTICS_FLAGS_HC_OUT_UCAST_PACKETS
    | STATISTICS_FLAGS_HC_OUT_UCAST_OCTETS
    | STATISTICS_FLAGS_HC_OUT_MCAST_PACKETS
    | STATISTICS_FLAGS_HC_OUT_MCAST_OCTETS
    | STATISTICS_FLAGS_HC_OUT_BCAST_PACKETS
    | STATISTICS_FLAGS_HC_OUT_BCAST_OCTETS;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryStatisticsRequest {
    pub requested_statistics: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ManaQueryStatisticsResponse {
    pub reported_statistics: u64,
    // In discards/errors
    pub in_discards_no_wqe: u64,
    pub in_errors_rx_vport_disabled: u64,
    // In bytes/packets
    pub hc_in_octets: u64,
    pub hc_in_ucast_pkts: u64,
    pub hc_in_ucast_octets: u64,
    pub hc_in_multicast_pkts: u64,
    pub hc_in_multicast_octets: u64,
    pub hc_in_broadcast_pkts: u64,
    pub hc_in_broadcast_octets: u64,
    // Out errors
    pub out_errors_gf_disabled: u64,
    pub out_errors_vport_disabled: u64,
    pub out_errors_invalid_vport_offset_packets: u64,
    pub out_errors_vlan_enforcement: u64,
    pub out_errors_eth_type_enforcement: u64,
    pub out_errors_sa_enforcement: u64,
    pub out_errors_sqpdid_enforcement: u64,
    pub out_errors_cqpdid_enforcement: u64,
    pub out_errors_mtu_violation: u64,
    pub out_errors_invalid_oob: u64,
    // Out bytes/packets
    pub hc_out_octets: u64,
    pub hc_out_ucast_pkts: u64,
    pub hc_out_ucast_octets: u64,
    pub hc_out_multicast_pkts: u64,
    pub hc_out_multicast_octets: u64,
    pub hc_out_broadcast_pkts: u64,
    pub hc_out_broadcast_octets: u64,
}
