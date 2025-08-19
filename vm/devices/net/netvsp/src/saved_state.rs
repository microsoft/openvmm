// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Saved state definitions.

use mesh::payload::Protobuf;
use vmbus_channel::gpadl::GpadlId;
use vmcore::save_restore::SavedStateRoot;

#[derive(Debug, Protobuf, SavedStateRoot)]
#[mesh(package = "net.netvsp")]
pub struct SavedState {
    #[mesh(1)]
    pub open: Option<OpenState>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct OpenState {
    #[mesh(1)]
    pub primary: Primary,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub enum Primary {
    #[mesh(1)]
    Version,
    #[mesh(2)]
    Init(#[mesh(1)] InitPrimary),
    #[mesh(3)]
    Ready(#[mesh(1)] ReadyPrimary),
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct InitPrimary {
    #[mesh(1)]
    pub version: u32,
    #[mesh(2)]
    pub ndis_config: Option<NdisConfig>,
    #[mesh(3)]
    pub ndis_version: Option<NdisVersion>,
    #[mesh(4)]
    pub receive_buffer: Option<ReceiveBuffer>,
    #[mesh(5)]
    pub send_buffer: Option<SendBuffer>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct NdisVersion {
    #[mesh(1)]
    pub major: u32,
    #[mesh(2)]
    pub minor: u32,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct NdisConfig {
    #[mesh(1)]
    pub mtu: u32,
    #[mesh(2)]
    pub capabilities: u64,
}

#[derive(Copy, Clone, Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub enum GuestVfState {
    #[mesh(1)]
    NoState,
    #[mesh(2)]
    AvailableAdvertised,
    #[mesh(3)]
    Ready,
    #[mesh(4)]
    DataPathSwitchPending {
        #[mesh(1)]
        to_guest: bool,
        #[mesh(2)]
        id: Option<u64>,
        #[mesh(3)]
        result: Option<bool>,
    },
    #[mesh(5)]
    DataPathSwitched,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct ReadyPrimary {
    #[mesh(1)]
    pub version: u32,
    #[mesh(2)]
    pub receive_buffer: ReceiveBuffer,
    #[mesh(3)]
    pub send_buffer: Option<SendBuffer>,
    #[mesh(4)]
    pub ndis_config: NdisConfig,
    #[mesh(5)]
    pub ndis_version: NdisVersion,
    #[mesh(6)]
    pub rndis_state: RndisState,
    #[mesh(7)]
    pub guest_vf_state: GuestVfState,
    #[mesh(8)]
    pub offload_config: OffloadConfig,
    #[mesh(9)]
    pub pending_offload_change: bool,
    #[mesh(10)]
    pub control_messages: Vec<IncomingControlMessage>,
    #[mesh(11)]
    pub rss_state: Option<RssState>,
    #[mesh(12)]
    pub channels: Vec<Option<Channel>>,
    #[mesh(13)]
    pub tx_spread_sent: bool,
    #[mesh(14)]
    pub guest_link_down: bool,
    #[mesh(15)]
    pub pending_link_action: Option<bool>,
    #[mesh(16)]
    pub packet_filter: Option<u32>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub enum RndisState {
    #[mesh(1)]
    Initializing,
    #[mesh(2)]
    Operational,
    #[mesh(3)]
    Halted,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct OffloadConfig {
    #[mesh(1)]
    pub checksum_tx: ChecksumOffloadConfig,
    #[mesh(2)]
    pub checksum_rx: ChecksumOffloadConfig,
    #[mesh(3)]
    pub lso4: bool,
    #[mesh(4)]
    pub lso6: bool,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct ChecksumOffloadConfig {
    #[mesh(1)]
    pub ipv4_header: bool,
    #[mesh(2)]
    pub tcp4: bool,
    #[mesh(3)]
    pub udp4: bool,
    #[mesh(4)]
    pub tcp6: bool,
    #[mesh(5)]
    pub udp6: bool,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct RssState {
    #[mesh(1)]
    pub key: Vec<u8>,
    #[mesh(2)]
    pub indirection_table: Vec<u16>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct IncomingControlMessage {
    #[mesh(1)]
    pub message_type: u32,
    #[mesh(2)]
    pub data: Vec<u8>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct ReceiveBuffer {
    #[mesh(1)]
    pub gpadl_id: GpadlId,
    #[mesh(2)]
    pub id: u16,
    #[mesh(3)]
    pub sub_allocation_size: u32,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct SendBuffer {
    #[mesh(1)]
    pub gpadl_id: GpadlId,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct Channel {
    #[mesh(1)]
    pub pending_tx_completions: Vec<u64>,
    #[mesh(2)]
    pub in_use_rx: Vec<Rx>,
}

#[derive(Debug, Protobuf)]
#[mesh(package = "net.netvsp")]
pub struct Rx {
    #[mesh(1)]
    pub buffers: Vec<u32>,
}
