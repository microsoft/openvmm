// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]

use bitfield_struct::bitfield;
use open_enum::open_enum;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

//
//  Basic types
//

type RequestId = u32;
type Handle = u32;
type Status = u32;
type ClassId = u32;
type Medium = u32;
type AddressFamily = u32;

//
//  Status codes
//

pub const STATUS_SUCCESS: Status = 0x00000000;
pub const STATUS_PENDING: Status = 0x00000103;
pub const STATUS_NOT_RECOGNIZED: Status = 0x00010001;
pub const STATUS_NOT_COPIED: Status = 0x00010002;
pub const STATUS_NOT_ACCEPTED: Status = 0x00010003;
pub const STATUS_CALL_ACTIVE: Status = 0x00010007;
pub const STATUS_ONLINE: Status = 0x40010003;
pub const STATUS_RESET_START: Status = 0x40010004;
pub const STATUS_RESET_END: Status = 0x40010005;
pub const STATUS_RING_STATUS: Status = 0x40010006;
pub const STATUS_CLOSED: Status = 0x40010007;
pub const STATUS_WAN_LINE_UP: Status = 0x40010008;
pub const STATUS_WAN_LINE_DOWN: Status = 0x40010009;
pub const STATUS_WAN_FRAGMENT: Status = 0x4001000A;
pub const STATUS_MEDIA_CONNECT: Status = 0x4001000B;
pub const STATUS_MEDIA_DISCONNECT: Status = 0x4001000C;
pub const STATUS_HARDWARE_LINE_UP: Status = 0x4001000D;
pub const STATUS_HARDWARE_LINE_DOWN: Status = 0x4001000E;
pub const STATUS_INTERFACE_UP: Status = 0x4001000F;
pub const STATUS_INTERFACE_DOWN: Status = 0x40010010;
pub const STATUS_MEDIA_BUSY: Status = 0x40010011;
pub const STATUS_MEDIA_SPECIFIC_INDICATION: Status = 0x40010012;
pub const STATUS_LINK_SPEED_CHANGE: Status = 0x40010013;
pub const STATUS_TASK_OFFLOAD_CURRENT_CONFIG: Status = 0x40020006;
pub const STATUS_NOT_RESETTABLE: Status = 0x80010001;
pub const STATUS_SOFT_ERRORS: Status = 0x80010003;
pub const STATUS_HARD_ERRORS: Status = 0x80010004;
pub const STATUS_BUFFER_OVERFLOW: Status = 0x80000005;
pub const STATUS_FAILURE: Status = 0xC0000001;
pub const STATUS_RESOURCES: Status = 0xC000009A;
pub const STATUS_CLOSING: Status = 0xC0010002;
pub const STATUS_BAD_VERSION: Status = 0xC0010004;
pub const STATUS_BAD_CHARACTERISTICS: Status = 0xC0010005;
pub const STATUS_ADAPTER_NOT_FOUND: Status = 0xC0010006;
pub const STATUS_OPEN_FAILED: Status = 0xC0010007;
pub const STATUS_DEVICE_FAILED: Status = 0xC0010008;
pub const STATUS_MULTICAST_FULL: Status = 0xC0010009;
pub const STATUS_MULTICAST_EXISTS: Status = 0xC001000A;
pub const STATUS_MULTICAST_NOT_FOUND: Status = 0xC001000B;
pub const STATUS_REQUEST_ABORTED: Status = 0xC001000C;
pub const STATUS_RESET_IN_PROGRESS: Status = 0xC001000D;
pub const STATUS_CLOSING_INDICATING: Status = 0xC001000E;
pub const STATUS_NOT_SUPPORTED: Status = 0xC00000BB;
pub const STATUS_INVALID_PACKET: Status = 0xC001000F;
pub const STATUS_OPEN_LIST_FULL: Status = 0xC0010010;
pub const STATUS_ADAPTER_NOT_READY: Status = 0xC0010011;
pub const STATUS_ADAPTER_NOT_OPEN: Status = 0xC0010012;
pub const STATUS_NOT_INDICATING: Status = 0xC0010013;
pub const STATUS_INVALID_LENGTH: Status = 0xC0010014;
pub const STATUS_INVALID_DATA: Status = 0xC0010015;
pub const STATUS_BUFFER_TOO_SHORT: Status = 0xC0010016;
pub const STATUS_INVALID_OID: Status = 0xC0010017;
pub const STATUS_ADAPTER_REMOVED: Status = 0xC0010018;
pub const STATUS_UNSUPPORTED_MEDIA: Status = 0xC0010019;
pub const STATUS_GROUP_ADDRESS_IN_USE: Status = 0xC001001A;
pub const STATUS_FILE_NOT_FOUND: Status = 0xC001001B;
pub const STATUS_ERROR_READING_FILE: Status = 0xC001001C;
pub const STATUS_ALREADY_MAPPED: Status = 0xC001001D;
pub const STATUS_RESOURCE_CONFLICT: Status = 0xC001001E;
pub const STATUS_NO_CABLE: Status = 0xC001001F;
pub const STATUS_INVALID_SAP: Status = 0xC0010020;
pub const STATUS_SAP_IN_USE: Status = 0xC0010021;
pub const STATUS_INVALID_ADDRESS: Status = 0xC0010022;
pub const STATUS_VC_NOT_ACTIVATED: Status = 0xC0010023;
pub const STATUS_DEST_OUT_OF_ORDER: Status = 0xC0010024;
pub const STATUS_VC_NOT_AVAILABLE: Status = 0xC0010025;
pub const STATUS_CELLRATE_NOT_AVAILABLE: Status = 0xC0010026;
pub const STATUS_INCOMPATIBLE_QOS: Status = 0xC0010027;
pub const STATUS_AAL_PARAMS_UNSUPPORTED: Status = 0xC0010028;
pub const STATUS_NO_ROUTE_TO_DESTINATION: Status = 0xC0010029;
pub const STATUS_TOKEN_RING_OPEN_ERROR: Status = 0xC0011000;

//
// Object Identifiers used by NdisRequest Query/Set Information
//

//
// General Objects
//

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Oid: u32 {
        OID_GEN_SUPPORTED_LIST = 0x00010101,
        OID_GEN_HARDWARE_STATUS = 0x00010102,
        OID_GEN_MEDIA_SUPPORTED = 0x00010103,
        OID_GEN_MEDIA_IN_USE = 0x00010104,
        OID_GEN_MAXIMUM_LOOKAHEAD = 0x00010105,
        OID_GEN_MAXIMUM_FRAME_SIZE = 0x00010106,
        OID_GEN_LINK_SPEED = 0x00010107,
        OID_GEN_TRANSMIT_BUFFER_SPACE = 0x00010108,
        OID_GEN_RECEIVE_BUFFER_SPACE = 0x00010109,
        OID_GEN_TRANSMIT_BLOCK_SIZE = 0x0001010A,
        OID_GEN_RECEIVE_BLOCK_SIZE = 0x0001010B,
        OID_GEN_VENDOR_ID = 0x0001010C,
        OID_GEN_VENDOR_DESCRIPTION = 0x0001010D,
        OID_GEN_CURRENT_PACKET_FILTER = 0x0001010E,
        OID_GEN_CURRENT_LOOKAHEAD = 0x0001010F,
        OID_GEN_DRIVER_VERSION = 0x00010110,
        OID_GEN_MAXIMUM_TOTAL_SIZE = 0x00010111,
        OID_GEN_PROTOCOL_OPTIONS = 0x00010112,
        OID_GEN_MAC_OPTIONS = 0x00010113,
        OID_GEN_MEDIA_CONNECT_STATUS = 0x00010114,
        OID_GEN_MAXIMUM_SEND_PACKETS = 0x00010115,
        OID_GEN_VENDOR_DRIVER_VERSION = 0x00010116,
        OID_GEN_NETWORK_LAYER_ADDRESSES = 0x00010118,
        OID_GEN_TRANSPORT_HEADER_OFFSET = 0x00010119,
        OID_GEN_RECEIVE_SCALE_CAPABILITIES = 0x00010203,
        OID_GEN_RECEIVE_SCALE_PARAMETERS = 0x00010204,
        OID_GEN_MAX_LINK_SPEED = 0x00010206,
        OID_GEN_LINK_STATE = 0x00010207,
        OID_GEN_LINK_PARAMETERS = 0x00010208,
        OID_GEN_INTERRUPT_MODERATION = 0x00010209,
        OID_GEN_MACHINE_NAME = 0x0001021A,
        OID_GEN_RNDIS_CONFIG_PARAMETER = 0x0001021B,

        OID_GEN_XMIT_OK = 0x00020101,
        OID_GEN_RCV_OK = 0x00020102,
        OID_GEN_XMIT_ERROR = 0x00020103,
        OID_GEN_RCV_ERROR = 0x00020104,
        OID_GEN_RCV_NO_BUFFER = 0x00020105,

        OID_GEN_DIRECTED_BYTES_XMIT = 0x00020201,
        OID_GEN_DIRECTED_FRAMES_XMIT = 0x00020202,
        OID_GEN_MULTICAST_BYTES_XMIT = 0x00020203,
        OID_GEN_MULTICAST_FRAMES_XMIT = 0x00020204,
        OID_GEN_BROADCAST_BYTES_XMIT = 0x00020205,
        OID_GEN_BROADCAST_FRAMES_XMIT = 0x00020206,
        OID_GEN_DIRECTED_BYTES_RCV = 0x00020207,
        OID_GEN_DIRECTED_FRAMES_RCV = 0x00020208,
        OID_GEN_MULTICAST_BYTES_RCV = 0x00020209,
        OID_GEN_BYTES_RCV = 0x00020219,
        OID_GEN_MULTICAST_FRAMES_RCV = 0x0002020A,
        OID_GEN_BROADCAST_BYTES_RCV = 0x0002020B,
        OID_GEN_BROADCAST_FRAMES_RCV = 0x0002020C,
        OID_GEN_BYTES_XMIT = 0x0002021A,
        OID_GEN_RCV_DISCARDS = 0x0002021B,
        OID_GEN_XMIT_DISCARDS = 0x0002021C,
        OID_TCP_RSC_STATISTICS = 0x0002021D,

        OID_GEN_RCV_CRC_ERROR = 0x0002020D,
        OID_GEN_TRANSMIT_QUEUE_LENGTH = 0x0002020E,

        OID_GEN_GET_TIME_CAPS = 0x0002020F,
        OID_GEN_GET_NETCARD_TIME = 0x00020210,
        OID_GEN_FRIENDLY_NAME = 0x00020216,

        //
        // 802.3 Objects (Ethernet)
        //

        OID_802_3_PERMANENT_ADDRESS = 0x01010101,
        OID_802_3_CURRENT_ADDRESS = 0x01010102,
        OID_802_3_MULTICAST_LIST = 0x01010103,
        OID_802_3_MAXIMUM_LIST_SIZE = 0x01010104,
        OID_802_3_MAC_OPTIONS = 0x01010105,
        OID_OFFLOAD_ENCAPSULATION = 0x0101010A,
        OID_802_3_ADD_MULTICAST_ADDRESS = 0x01010208,
        OID_802_3_DELETE_MULTICAST_ADDRESS = 0x01010209,

        OID_802_3_RCV_ERROR_ALIGNMENT = 0x01020101,
        OID_802_3_XMIT_ONE_COLLISION = 0x01020102,
        OID_802_3_XMIT_MORE_COLLISIONS = 0x01020103,

        OID_802_3_XMIT_DEFERRED = 0x01020201,
        OID_802_3_XMIT_MAX_COLLISIONS = 0x01020202,
        OID_802_3_RCV_OVERRUN = 0x01020203,
        OID_802_3_XMIT_UNDERRUN = 0x01020204,
        OID_802_3_XMIT_HEARTBEAT_FAILURE = 0x01020205,
        OID_802_3_XMIT_TIMES_CRS_LOST = 0x01020206,
        OID_802_3_XMIT_LATE_COLLISIONS = 0x01020207,

        OID_TCP_OFFLOAD_CURRENT_CONFIG = 0xFC01020B,
        OID_TCP_OFFLOAD_PARAMETERS = 0xFC01020C,
        OID_TCP_OFFLOAD_HARDWARE_CAPABILITIES = 0xFC01020D,
        OID_TCP_CONNECTION_OFFLOAD_CURRENT_CONFIG = 0xFC01020E,
        OID_TCP_CONNECTION_OFFLOAD_HARDWARE_CAPABILITIES = 0xFC01020F,
    }
}

/// Response to OID_GEN_FRIENDLY_NAME.
#[repr(C)]
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct FriendlyName {
    pub name: [u16; 255],
    pub null: u16,
}

//
// Remote NDIS message types
//
pub const MESSAGE_TYPE_PACKET_MSG: u32 = 0x00000001;
pub const MESSAGE_TYPE_INITIALIZE_MSG: u32 = 0x00000002;
pub const MESSAGE_TYPE_HALT_MSG: u32 = 0x00000003;
pub const MESSAGE_TYPE_QUERY_MSG: u32 = 0x00000004;
pub const MESSAGE_TYPE_SET_MSG: u32 = 0x00000005;
pub const MESSAGE_TYPE_RESET_MSG: u32 = 0x00000006;
pub const MESSAGE_TYPE_INDICATE_STATUS_MSG: u32 = 0x00000007;
pub const MESSAGE_TYPE_KEEPALIVE_MSG: u32 = 0x00000008;
pub const MESSAGE_TYPE_SET_EX_MSG: u32 = 0x00000009;

// Remote NDIS message completion types
pub const MESSAGE_TYPE_INITIALIZE_CMPLT: u32 = 0x80000002;
pub const MESSAGE_TYPE_QUERY_CMPLT: u32 = 0x80000004;
pub const MESSAGE_TYPE_SET_CMPLT: u32 = 0x80000005;
pub const MESSAGE_TYPE_RESET_CMPLT: u32 = 0x80000006;
pub const MESSAGE_TYPE_KEEPALIVE_CMPLT: u32 = 0x80000008;
pub const MESSAGE_TYPE_SET_EX_CMPLT: u32 = 0x80000009;

//
// Reserved message type for private communication between lower-layer
// host driver and remote device, if necessary.
//
pub const MESSAGE_TYPE_BUS_MSG: u32 = 0xff000001;

//
//  Defines for DeviceFlags in RNDIS_INITIALIZE_COMPLETE
//
pub const DF_CONNECTIONLESS: u32 = 0x00000001;
pub const DF_CONNECTION_ORIENTED: u32 = 0x00000002;
pub const DF_RAW_DATA: u32 = 0x00000004;

//
//  Remote NDIS medium types.
//
pub const MEDIUM_802_3: u32 = 0x00000000;
pub const MEDIUM_802_5: u32 = 0x00000001;
pub const MEDIUM_FDDI: u32 = 0x00000002;
pub const MEDIUM_WAN: u32 = 0x00000003;
pub const MEDIUM_LOCAL_TALK: u32 = 0x00000004;
pub const MEDIUM_ARCNET_RAW: u32 = 0x00000006;
pub const MEDIUM_ARCNET878_2: u32 = 0x00000007;
pub const MEDIUM_ATM: u32 = 0x00000008;
pub const MEDIUM_WIRELESS_WAN: u32 = 0x00000009;
pub const MEDIUM_IRDA: u32 = 0x0000000a;
pub const MEDIUM_CO_WAN: u32 = 0x0000000b;
pub const MEDIUM_MAX: u32 = 0x0000000d; // Not a real medium, defined as an upper-bound

//
// Remote NDIS medium connection states.
//
pub const MEDIA_STATE_CONNECTED: u32 = 0x00000000;
pub const MEDIA_STATE_DISCONNECTED: u32 = 0x00000001;

//
//  Remote NDIS version numbers
//
pub const MAJOR_VERSION: u32 = 0x00000001;
pub const MINOR_VERSION: u32 = 0x00000000;

//
// Ndis MAC option bits (OID_GEN_MAC_OPTIONS).
//
pub const MAC_OPTION_COPY_LOOKAHEAD_DATA: u32 = 0x00000001;
pub const MAC_OPTION_RECEIVE_SERIALIZED: u32 = 0x00000002;
pub const MAC_OPTION_TRANSFERS_NOT_PEND: u32 = 0x00000004;
pub const MAC_OPTION_NO_LOOPBACK: u32 = 0x00000008;
pub const MAC_OPTION_FULL_DUPLEX: u32 = 0x00000010;
pub const MAC_OPTION_EOTX_INDICATION: u32 = 0x00000020;
pub const MAC_OPTION_8021P_PRIORITY: u32 = 0x00000040;
pub const MAC_OPTION_SUPPORTS_MAC_ADDRESS_OVERWRITE: u32 = 0x00000080;
pub const MAC_OPTION_RECEIVE_AT_DPC: u32 = 0x00000100;
pub const MAC_OPTION_8021Q_VLAN: u32 = 0x00000200;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LinkState {
    pub header: NdisObjectHeader,
    pub media_connect_state: u32,
    pub media_duplex_state: u32,
    pub padding: u32,
    pub xmit_link_speed: u64,
    pub rcv_link_speed: u64,
    pub pause_functions: u32,
    pub auto_negotiation_flags: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LinkSpeed {
    pub xmit: u64,
    pub rcv: u64,
}

//
//  NdisInitialize message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InitializeRequest {
    pub request_id: RequestId,
    pub major_version: u32,
    pub minor_version: u32,
    pub max_transfer_size: u32,
}

//
//  Response to NdisInitialize
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct InitializeComplete {
    pub request_id: RequestId,
    pub status: Status,
    pub major_version: u32,
    pub minor_version: u32,
    pub device_flags: u32,
    pub medium: Medium,
    pub max_packets_per_message: u32,
    pub max_transfer_size: u32,
    pub packet_alignment_factor: u32,
    pub af_list_offset: u32,
    pub af_list_size: u32,
}

//
//  Call manager devices only: Information about an address family
//  supported by the device is appended to the response to NdisInitialize.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CoAddressFamily {
    pub address_family: AddressFamily,
    pub major_version: u32,
    pub minor_version: u32,
}

//
//  NdisHalt message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HaltRequest {
    pub request_id: RequestId,
}

//
// NdisQueryRequest message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryRequest {
    pub request_id: RequestId,
    pub oid: Oid,
    pub information_buffer_length: u32,
    pub information_buffer_offset: u32,
    pub device_vc_handle: Handle,
}

//
//  Response to NdisQueryRequest
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryComplete {
    pub request_id: RequestId,
    pub status: Status,
    pub information_buffer_length: u32,
    pub information_buffer_offset: u32,
}

//
//  NdisSetRequest message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetRequest {
    pub request_id: RequestId,
    pub oid: Oid,
    pub information_buffer_length: u32,
    pub information_buffer_offset: u32,
    pub device_vc_handle: Handle,
}

//
//  Response to NdisSetRequest
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetComplete {
    pub request_id: RequestId,
    pub status: Status,
}

//
//  NdisSetExRequest message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetExRequest {
    pub request_id: RequestId,
    pub oid: Oid,
    pub information_buffer_length: u32,
    pub information_buffer_offset: u32,
    pub device_vc_handle: Handle,
}

//
//  Response to NdisSetExRequest
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetExComplete {
    pub request_id: RequestId,
    pub status: Status,
    pub information_buffer_length: u32,
    pub information_buffer_offset: u32,
}

//
//  NdisReset message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ResetRequest {
    pub reserved: u32,
}

//
//  Response to NdisReset
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ResetComplete {
    pub status: Status,
    pub addressing_reset: u32,
}

//
//  NdisMIndicateStatus message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IndicateStatus {
    pub status: Status,
    pub status_buffer_length: u32,
    pub status_buffer_offset: u32,
}

//
//  Diagnostic information passed as the status buffer in
//  RNDIS_INDICATE_STATUS messages signifying error conditions.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DiagnosticInfo {
    pub diag_status: Status,
    pub error_offset: u32,
}

//
//  NdisKeepAlive message
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeepaliveRequest {
    pub request_id: RequestId,
}

//
// Response to NdisKeepAlive
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeepaliveComplete {
    pub request_id: RequestId,
    pub status: Status,
}

//
//  Data message. All Offset fields contain byte offsets from the beginning
//  of the RNDIS_PACKET structure. All Length fields are in bytes.
//  VcHandle is set to 0 for connectionless data, otherwise it
//  contains the VC handle.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Packet {
    pub data_offset: u32,
    pub data_length: u32,
    pub oob_data_offset: u32,
    pub oob_data_length: u32,
    pub num_oob_data_elements: u32,
    pub per_packet_info_offset: u32,
    pub per_packet_info_length: u32,
    pub vc_handle: Handle,
    pub reserved: u32,
}

//
//  Optional Out of Band data associated with a Data message.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Oobd {
    pub size: u32,
    pub typ: ClassId,
    pub class_information_offset: u32,
}

pub const PACKET_INFO_FLAGS_MULTI_SUBALLOC: u8 = 1 << 0;
pub const PACKET_INFO_FLAGS_MULTI_SUBALLOC_FIRST_FRAGMENT: u8 = 1 << 1;
pub const PACKET_INFO_FLAGS_MULTI_SUBALLOC_LAST_FRAGMENT: u8 = 1 << 2;

pub const PACKET_INFO_ID_VERSION_V1: u8 = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PacketIdInfo {
    pub version: u8,
    pub flags: u8,
    pub packet_id: u16,
}

const PACKET_INFO_ID: u16 = 1;

//
//  Packet extension field contents associated with a Data message.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PerPacketInfo {
    pub size: u32,
    pub typ: u32, // high bit means internal
    pub per_packet_information_offset: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TxTcpIpChecksumInfo(pub u32);

impl TxTcpIpChecksumInfo {
    pub fn is_ipv4(self) -> bool {
        self.0 & (1 << 0) != 0
    }
    pub fn set_is_ipv4(mut self, v: bool) -> Self {
        self.0 &= !(1 << 0);
        self.0 |= v as u32;
        self
    }
    pub fn is_ipv6(self) -> bool {
        self.0 & (1 << 1) != 0
    }
    pub fn set_is_ipv6(mut self, v: bool) -> Self {
        self.0 &= !(1 << 1);
        self.0 |= (v as u32) << 1;
        self
    }
    pub fn tcp_checksum(self) -> bool {
        self.0 & (1 << 2) != 0
    }
    pub fn set_tcp_checksum(mut self, v: bool) -> Self {
        self.0 &= !(1 << 2);
        self.0 |= (v as u32) << 2;
        self
    }
    pub fn udp_checksum(self) -> bool {
        self.0 & (1 << 3) != 0
    }
    pub fn set_udp_checksum(mut self, v: bool) -> Self {
        self.0 &= !(1 << 3);
        self.0 |= (v as u32) << 3;
        self
    }
    pub fn ip_header_checksum(self) -> bool {
        self.0 & (1 << 4) != 0
    }
    pub fn set_ip_header_checksum(mut self, v: bool) -> Self {
        self.0 &= !(1 << 4);
        self.0 |= (v as u32) << 4;
        self
    }
    pub fn tcp_header_offset(self) -> u16 {
        ((self.0 >> 16) as u16) & 0x3ff
    }
    pub fn set_tcp_header_offset(mut self, v: u16) -> Self {
        self.0 &= !0x3ff0000;
        self.0 |= (v as u32 & 0x3ff) << 16;
        self
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RxTcpIpChecksumInfo(pub u32);

impl RxTcpIpChecksumInfo {
    pub fn tcp_checksum_failed(self) -> bool {
        self.0 & (1 << 0) != 0
    }
    pub fn set_tcp_checksum_failed(mut self, v: bool) -> Self {
        self.0 &= !(1 << 0);
        self.0 |= v as u32;
        self
    }
    pub fn udp_checksum_failed(self) -> bool {
        self.0 & (1 << 1) != 0
    }
    pub fn set_udp_checksum_failed(mut self, v: bool) -> Self {
        self.0 &= !(1 << 1);
        self.0 |= (v as u32) << 1;
        self
    }
    pub fn ip_checksum_failed(self) -> bool {
        self.0 & (1 << 2) != 0
    }
    pub fn set_ip_checksum_failed(mut self, v: bool) -> Self {
        self.0 &= !(1 << 2);
        self.0 |= (v as u32) << 2;
        self
    }
    pub fn tcp_checksum_succeeded(self) -> bool {
        self.0 & (1 << 3) != 0
    }
    pub fn set_tcp_checksum_succeeded(mut self, v: bool) -> Self {
        self.0 &= !(1 << 3);
        self.0 |= (v as u32) << 3;
        self
    }
    pub fn udp_checksum_succeeded(self) -> bool {
        self.0 & (1 << 4) != 0
    }
    pub fn set_udp_checksum_succeeded(mut self, v: bool) -> Self {
        self.0 &= !(1 << 4);
        self.0 |= (v as u32) << 4;
        self
    }
    pub fn ip_checksum_succeeded(self) -> bool {
        self.0 & (1 << 5) != 0
    }
    pub fn set_ip_checksum_succeeded(mut self, v: bool) -> Self {
        self.0 &= !(1 << 5);
        self.0 |= (v as u32) << 5;
        self
    }
    pub fn loopback(self) -> bool {
        self.0 & (1 << 6) != 0
    }
    pub fn set_loopback(mut self, v: bool) -> Self {
        self.0 &= !(1 << 6);
        self.0 |= (v as u32) << 6;
        self
    }
    pub fn tcp_checksum_value_invalid(self) -> bool {
        self.0 & (1 << 7) != 0
    }
    pub fn set_tcp_checksum_value_invalid(mut self, v: bool) -> Self {
        self.0 &= !(1 << 7);
        self.0 |= (v as u32) << 7;
        self
    }
    pub fn ip_checksum_value_invalid(self) -> bool {
        self.0 & (1 << 8) != 0
    }
    pub fn set_ip_checksum_value_invalid(mut self, v: bool) -> Self {
        self.0 &= !(1 << 8);
        self.0 |= (v as u32) << 8;
        self
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TcpLsoInfo(pub u32);

impl TcpLsoInfo {
    pub fn mss(self) -> u32 {
        self.0 & 0xfffff
    }

    pub fn tcp_header_offset(self) -> u16 {
        (self.0 >> 20) as u16 & 0x3ff
    }

    pub fn is_ipv4(self) -> bool {
        self.0 & (1 << 31) == 0
    }

    pub fn is_ipv6(self) -> bool {
        !self.is_ipv4()
    }
}

pub const PPI_TCP_IP_CHECKSUM: u32 = 0;
pub const PPI_LSO: u32 = 2;

//
//  Format of Information buffer passed in a SetRequest for the OID
//  OID_GEN_RNDIS_CONFIG_PARAMETER.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ConfigParameterInfo {
    pub parameter_name_offset: u32,
    pub parameter_name_length: u32,
    pub parameter_type: u32,
    pub parameter_value_offset: u32,
    pub parameter_value_length: u32,
}

//
//  Values for ParameterType in ConfigParameterInfo
//
pub const CONFIG_PARAM_TYPE_INTEGER: u32 = 0;
pub const CONFIG_PARAM_TYPE_STRING: u32 = 2;

//
// Remote NDIS message format
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub message_type: u32,

    // Total length of this message, from the beginning
    // of the header struct, in bytes.
    pub message_length: u32,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum NdisObjectType: u8 {
        DEFAULT = 0x80,
        RSS_CAPABILITIES = 0x88,
        RSS_PARAMETERS = 0x89,
        OID_REQUEST = 0x96,
        OFFLOAD = 0xA7,
        OFFLOAD_ENCAPSULATION = 0xA8,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisObjectHeader {
    pub object_type: NdisObjectType,
    pub revision: u8,
    pub size: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisReceiveScaleCapabilities {
    pub header: NdisObjectHeader,
    pub capabilities_flags: u32,
    pub number_of_interrupt_messages: u32,
    pub number_of_receive_queues: u32,
    pub number_of_indirection_table_entries: u16,
    pub padding: u16,
}

pub const NDIS_SIZEOF_RECEIVE_SCALE_CAPABILITIES_REVISION_2: usize = 18;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisReceiveScaleParameters {
    pub header: NdisObjectHeader,

    // Qualifies the rest of the information.
    pub flags: u16,

    // The base CPU number to do receive processing. not used.
    pub base_cpu_number: u16,

    // This describes the hash function and type being enabled.
    pub hash_information: u32,

    // The size of indirection table array.
    pub indirection_table_size: u16,
    pub pad0: u16,
    // The offset of the indirection table from the beginning of this structure.
    pub indirection_table_offset: u32,

    // The size of the secret key.
    pub hash_secret_key_size: u16,
    pub pad1: u16,
    // The offset of the secret key from the beginning of this structure.
    pub hash_secret_key_offset: u32,

    // Array of type GROUP_AFFINITY representing procs used in the indirection table
    pub processor_masks_offset: u32,
    pub number_of_processor_masks: u32,
    pub processor_masks_entry_size: u32,

    // The hash map table is a CCHAR array for Revision 1.
    // It is a PROCESSOR_NUMBER array for Revision 2

    // Specifies default RSS processor.
    pub default_processor_number: u32,
}

pub const NDIS_SIZEOF_RECEIVE_SCALE_PARAMETERS_REVISION_1: usize = 28;
pub const NDIS_SIZEOF_RECEIVE_SCALE_PARAMETERS_REVISION_2: usize = 40;
pub const NDIS_SIZEOF_RECEIVE_SCALE_PARAMETERS_REVISION_3: usize = 44;

// Flags to denote the parameters that are kept unmodified.
pub const NDIS_RSS_PARAM_FLAG_BASE_CPU_UNCHANGED: u16 = 0x0001;
pub const NDIS_RSS_PARAM_FLAG_HASH_INFO_UNCHANGED: u16 = 0x0002;
pub const NDIS_RSS_PARAM_FLAG_ITABLE_UNCHANGED: u16 = 0x0004;
pub const NDIS_RSS_PARAM_FLAG_HASH_KEY_UNCHANGED: u16 = 0x0008;
pub const NDIS_RSS_PARAM_FLAG_DISABLE_RSS: u16 = 0x0010;
pub const NDIS_RSS_PARAM_FLAG_DEFAULT_PROCESSOR_UNCHANGED: u16 = 0x0020;

pub const NDIS_RSS_INDIRECTION_TABLE_SIZE_REVISION_1: u8 = 128;
pub const NDIS_RSS_HASH_SECRET_KEY_SIZE_REVISION_1: u8 = 40;

pub const NDIS_RSS_INDIRECTION_TABLE_MAX_SIZE_REVISION_1: usize = 128;
//pub const NDIS_RSS_INDIRECTION_TABLE_MAX_SIZE_REVISION_2: usize = (128 * sizeof(PROCESSOR_NUMBER));
//pub const NDIS_RSS_INDIRECTION_TABLE_MAX_SIZE_REVISION_3: usize = (128 * sizeof(PROCESSOR_NUMBER));

pub const NDIS_HASH_FUNCTION_MASK: u32 = 0x000000FF;
pub const NDIS_HASH_FUNCTION_TOEPLITZ: u32 = 0x00000001;

pub const NDIS_HASH_IPV4: u32 = 0x00000100;
pub const NDIS_HASH_TCP_IPV4: u32 = 0x00000200;
pub const NDIS_HASH_IPV6: u32 = 0x00000400;
pub const NDIS_HASH_IPV6_EX: u32 = 0x00000800;
pub const NDIS_HASH_TCP_IPV6: u32 = 0x00001000;
pub const NDIS_HASH_TCP_IPV6_EX: u32 = 0x00002000;
pub const NDIS_HASH_UDP_IPV4: u32 = 0x00004000;
pub const NDIS_HASH_UDP_IPV6: u32 = 0x00008000;
pub const NDIS_HASH_UDP_IPV6_EX: u32 = 0x00010000;

pub const NDIS_RSS_CAPS_HASH_TYPE_TCP_IPV4: u32 = 0x00000100;
pub const NDIS_RSS_CAPS_HASH_TYPE_TCP_IPV6: u32 = 0x00000200;
pub const NDIS_RSS_CAPS_HASH_TYPE_TCP_IPV6_EX: u32 = 0x00000400;
pub const NDIS_RSS_CAPS_HASH_TYPE_UDP_IPV4: u32 = 0x00000800;
pub const NDIS_RSS_CAPS_HASH_TYPE_UDP_IPV6: u32 = 0x00001000;
pub const NDIS_RSS_CAPS_HASH_TYPE_UDP_IPV6_EX: u32 = 0x00002000;
pub const NDIS_RSS_CAPS_MESSAGE_SIGNALED_INTERRUPTS: u32 = 0x01000000;
pub const NDIS_RSS_CAPS_CLASSIFICATION_AT_ISR: u32 = 0x02000000;
pub const NDIS_RSS_CAPS_CLASSIFICATION_AT_DPC: u32 = 0x04000000;
pub const NDIS_RSS_CAPS_USING_MSI_X: u32 = 0x08000000;
pub const NDIS_RSS_CAPS_RSS_AVAILABLE_ON_PORTS: u32 = 0x10000000;
pub const NDIS_RSS_CAPS_SUPPORTS_MSI_X: u32 = 0x20000000;
pub const NDIS_RSS_CAPS_SUPPORTS_INDEPENDENT_ENTRY_MOVE: u32 = 0x40000000;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisOffload {
    pub header: NdisObjectHeader,

    // Checksum Offload information
    pub checksum: TcpIpChecksumOffload,

    // Large Send Offload information
    pub lso_v1: [u32; 4],

    // IPsec Offload Information
    pub ipsec_v1: [u32; 7],

    // Large Send Offload version 2Information
    pub lso_v2: TcpLargeSendOffloadV2,

    pub flags: u32,

    // IPsec offload V2
    pub ipsec_v2: [u32; 8],

    // Receive Segment Coalescing information
    pub rsc: u8,
    pub reserved: [u8; 3],

    // NVGRE Encapsulated packet task offload information
    pub encapsulated_packet_task_offload_gre: [u32; 2],

    // VXLAN Encapsulated packet task offload information
    pub encapsulated_packet_task_offload_vxlan: [u32; 5],

    // Enabled encapsulation types for Encapsulated packet task offload
    pub encapsulation_types: u8,

    pub padding: [u8; 3],
}

pub const NDIS_SIZEOF_NDIS_OFFLOAD_REVISION_1: usize =
    std::mem::offset_of!(NdisOffload, flags) + size_of::<u32>();
const_assert_eq!(NDIS_SIZEOF_NDIS_OFFLOAD_REVISION_1, 112);

pub const NDIS_SIZEOF_NDIS_OFFLOAD_REVISION_3: usize =
    std::mem::offset_of!(NdisOffload, encapsulated_packet_task_offload_gre) + size_of::<[u32; 2]>();
const_assert_eq!(NDIS_SIZEOF_NDIS_OFFLOAD_REVISION_3, 156);

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TcpIpChecksumOffload {
    pub ipv4_tx_encapsulation: u32,
    pub ipv4_tx_flags: Ipv4ChecksumOffload,
    pub ipv4_rx_encapsulation: u32,
    pub ipv4_rx_flags: Ipv4ChecksumOffload,
    pub ipv6_tx_encapsulation: u32,
    pub ipv6_tx_flags: Ipv6ChecksumOffload,
    pub ipv6_rx_encapsulation: u32,
    pub ipv6_rx_flags: Ipv6ChecksumOffload,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Ipv4ChecksumOffload {
    #[bits(2)]
    pub ip_options_supported: u32,
    #[bits(2)]
    pub tcp_options_supported: u32,
    #[bits(2)]
    pub tcp_checksum: u32,
    #[bits(2)]
    pub udp_checksum: u32,
    #[bits(2)]
    pub ip_checksum: u32,
    #[bits(22)]
    _reserved: u32,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Ipv6ChecksumOffload {
    #[bits(2)]
    pub ip_extension_headers_supported: u32,
    #[bits(2)]
    pub tcp_options_supported: u32,
    #[bits(2)]
    pub tcp_checksum: u32,
    #[bits(2)]
    pub udp_checksum: u32,
    #[bits(24)]
    _reserved: u32,
}

pub const NDIS_ENCAPSULATION_IEEE_802_3: u32 = 2;
// Use the same minimum as vmswitch.
pub const LSO_MIN_SEGMENT_COUNT: u32 = 2;
// Use the same maximum as vmswitch.
pub const LSO_MAX_OFFLOAD_SIZE: u32 = 0xF53C;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TcpLargeSendOffloadV2 {
    pub ipv4_encapsulation: u32,
    pub ipv4_max_offload_size: u32,
    pub ipv4_min_segment_count: u32,
    pub ipv6_encapsulation: u32,
    pub ipv6_max_offload_size: u32,
    pub ipv6_min_segment_count: u32,
    pub ipv6_flags: Ipv6LsoFlags,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Ipv6LsoFlags {
    #[bits(2)]
    pub ip_extension_headers_supported: u32,
    #[bits(2)]
    pub tcp_options_supported: u32,
    #[bits(28)]
    _reserved: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisOffloadEncapsulation {
    pub header: NdisObjectHeader,
    pub ipv4_enabled: u32,
    pub ipv4_encapsulation_type: u32,
    pub ipv4_header_size: u32,
    pub ipv6_enabled: u32,
    pub ipv6_encapsulation_type: u32,
    pub ipv6_header_size: u32,
}

pub const NDIS_SIZEOF_OFFLOAD_ENCAPSULATION_REVISION_1: usize = 28;

pub const NDIS_OFFLOAD_NOT_SUPPORTED: u32 = 0;
pub const NDIS_OFFLOAD_SUPPORTED: u32 = 1;

pub const NDIS_OFFLOAD_NO_CHANGE: u32 = 1;
pub const NDIS_OFFLOAD_SET_ON: u32 = 1;
pub const NDIS_OFFLOAD_SET_OFF: u32 = 2;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisOffloadParameters {
    pub header: NdisObjectHeader,
    pub ipv4_checksum: OffloadParametersChecksum,
    pub tcp4_checksum: OffloadParametersChecksum,
    pub udp4_checksum: OffloadParametersChecksum,
    pub tcp6_checksum: OffloadParametersChecksum,
    pub udp6_checksum: OffloadParametersChecksum,
    pub lsov1: OffloadParametersSimple,
    pub ipsec_v1: u8,
    pub lsov2_ipv4: OffloadParametersSimple,
    pub lsov2_ipv6: OffloadParametersSimple,
    pub tcp_connection_ipv4: u8,
    pub tcp_connection_ipv6: u8,
    pub reserved: u8,
    pub flags: u32,
}

pub const NDIS_SIZEOF_OFFLOAD_PARAMETERS_REVISION_1: usize = 20;

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum OffloadParametersChecksum: u8 {
        NO_CHANGE = 0,
        TX_RX_DISABLED = 1,
        TX_ENABLED_RX_DISABLED = 2,
        RX_ENABLED_TX_DISABLED = 3,
        TX_RX_ENABLED = 4,
    }
}

impl OffloadParametersChecksum {
    pub fn tx_rx(&self) -> Option<(bool, bool)> {
        match *self {
            Self::NO_CHANGE => None,
            Self::TX_RX_ENABLED => Some((true, true)),
            Self::TX_ENABLED_RX_DISABLED => Some((true, false)),
            Self::RX_ENABLED_TX_DISABLED => Some((false, true)),
            _ => None,
        }
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum OffloadParametersSimple: u8 {
        NO_CHANGE = 0,
        DISABLED = 1,
        ENABLED = 2,
    }
}

impl OffloadParametersSimple {
    pub fn enable(&self) -> Option<bool> {
        match *self {
            Self::NO_CHANGE => None,
            Self::ENABLED => Some(true),
            Self::DISABLED => Some(false),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RndisConfigParameterInfo {
    pub name_offset: u32,
    pub name_length: u32,
    pub parameter_type: NdisParameterType,
    pub value_offset: u32,
    pub value_length: u32,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum NdisParameterType: u32 {
        INTEGER = 0,
        HEX_INTEGER = 1,
        STRING = 2,
        MULTI_STRING = 3,
        BINARY = 4,
    }
}

pub type RndisPacketFilterOidValue = u32;

// Rndis Packet Filter Flags (OID_GEN_CURRENT_PACKET_FILTER)
pub const NDIS_PACKET_TYPE_NONE: u32 = 0x00000000;
pub const NDIS_PACKET_TYPE_DIRECTED: u32 = 0x00000001;
pub const NDIS_PACKET_TYPE_MULTICAST: u32 = 0x00000002;
pub const NDIS_PACKET_TYPE_ALL_MULTICAST: u32 = 0x00000004;
pub const NDIS_PACKET_TYPE_BROADCAST: u32 = 0x00000008;
pub const NPROTO_PACKET_FILTER: u32 =
    NDIS_PACKET_TYPE_DIRECTED | NDIS_PACKET_TYPE_ALL_MULTICAST | NDIS_PACKET_TYPE_BROADCAST;
