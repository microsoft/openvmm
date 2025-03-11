// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use crate::rndisprot;
use bitfield_struct::bitfield;
use inspect::Inspect;
use vmbus_channel::gpadl::GpadlId;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | minor as u32
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Inspect)]
pub enum Version {
    V1 = make_version(0, 2),
    V2 = make_version(3, 2),
    V4 = make_version(4, 0),
    V5 = make_version(5, 0),
    V6 = make_version(6, 0),
    V61 = make_version(6, 1),
}

impl Version {
    pub fn major(&self) -> u16 {
        (*self as u32 >> 16) as u16
    }
    pub fn minor(&self) -> u16 {
        *self as u16
    }
}

pub const PACKET_SIZE_V1: usize = 0x1c;
pub const PACKET_SIZE_V61: usize = 0x28;

pub const DATA_CHANNEL_TYPE: u32 = 0;
pub const CONTROL_CHANNEL_TYPE: u32 = 1;

pub const NVSP_OPERATIONAL_STATUS_OK: u32 = 0x00000000;
pub const NVSP_OPERATIONAL_STATUS_DEGRADED: u32 = 0x00000001;
pub const NVSP_OPERATIONAL_STATUS_NONRECOVERABLE: u32 = 0x00000002;
pub const NVSP_OPERATIONAL_STATUS_NO_CONTACT: u32 = 0x00000003;
pub const NVSP_OPERATIONAL_STATUS_LOST_COMMUNICATION: u32 = 0x00000004;

//
// The maximum number of transfer pages (packets) the VSP will use on a receive
//
pub const NVSP_MAX_PACKETS_PER_RECEIVE: u32 = 375;

// The maximum number of transfer pages (packets) the VSP will use on a receive
// when RSC Over VMBus is enabled
//  NVSP_MAX_PACKETS_PER_RECEIVE + (MAX_IPV4_PACKET / IPV4_MIN_MINIMUM_MTU) + 1 = 562
//
/*
pub const NVSP_MAX_PACKETS_PER_RECEIVE_RSC_OVER_VMBUS: u32 =
    NVSP_MAX_PACKETS_PER_RECEIVE + (MAX_IPV4_PACKET / IPV4_MIN_MINIMUM_MTU) + 1;
*/

//
// Defines the maximum number of indirection table entries that can be used
// by a single VMQ's traffic. We are storing this value here because both
// the VM and host needs it to manage the vRSS indirection table (VM needs
// it for send and host needs it for receive).
//
pub const VMS_SWITCH_RSS_MAX_INDIRECTION_TABLE_ENTRIES: u32 = 128;

//
// For vmNic, this defines the maximum number of send indirection table entries
// that can be used by a single VMQ. We are separating out the max table size
// for send and recv side indirection table, as updating send side
// indirection table size will require a bump in nvsp version.
//
pub const VMS_SWITCH_RSS_MAX_SEND_INDIRECTION_TABLE_ENTRIES: u32 = 16;

//
// Specified the minimum number of indirection table entries that can be used
// by a single VMQ's traffic.
//
pub const VMS_SWITCH_RSS_MIN_INDIRECTION_TABLE_ENTRIES: u32 = 16;

pub const MESSAGE_TYPE_NONE: u32 = 0;

//
// Init Messages
//
pub const MESSAGE_TYPE_INIT: u32 = 1;
pub const MESSAGE_TYPE_INIT_COMPLETE: u32 = 2;

pub const MESSAGE_START: u32 = 100;

//
// Version 1 Messages
//
pub const MESSAGE1_TYPE_SEND_NDIS_VERSION: u32 = MESSAGE_START;
pub const MESSAGE1_TYPE_SEND_RECEIVE_BUFFER: u32 = 101;
pub const MESSAGE1_TYPE_SEND_RECEIVE_BUFFER_COMPLETE: u32 = 102;
pub const MESSAGE1_TYPE_REVOKE_RECEIVE_BUFFER: u32 = 103;
pub const MESSAGE1_TYPE_SEND_SEND_BUFFER: u32 = 104;
pub const MESSAGE1_TYPE_SEND_SEND_BUFFER_COMPLETE: u32 = 105;
pub const MESSAGE1_TYPE_REVOKE_SEND_BUFFER: u32 = 106;
pub const MESSAGE1_TYPE_SEND_RNDIS_PACKET: u32 = 107;
pub const MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE: u32 = 108;

//
// The maximum allowed message ID for the v1 protocol.
//
pub const MESSAGE1_MAX: u32 = MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE;

//
// Version 2 messages
//
pub const MESSAGE2_TYPE_SEND_CHIMNEY_DELEGATED_BUFFER: u32 = 109;
pub const MESSAGE2_TYPE_SEND_CHIMNEY_DELEGATED_BUFFER_COMPLETE: u32 = 110;
pub const MESSAGE2_TYPE_REVOKE_CHIMNEY_DELEGATED_BUFFER: u32 = 111;
pub const MESSAGE2_TYPE_RESUME_CHIMNEY_RX_INDICATION: u32 = 112;
pub const MESSAGE2_TYPE_TERMINATE_CHIMNEY: u32 = 113;
pub const MESSAGE2_TYPE_TERMINATE_CHIMNEY_COMPLETE: u32 = 114;
pub const MESSAGE2_TYPE_INDICATE_CHIMNEY_EVENT: u32 = 115;
pub const MESSAGE2_TYPE_SEND_CHIMNEY_PACKET: u32 = 116;
pub const MESSAGE2_TYPE_SEND_CHIMNEY_PACKET_COMPLETE: u32 = 117;
pub const MESSAGE2_TYPE_POST_CHIMNEY_RECV_REQUEST: u32 = 118;
pub const MESSAGE2_TYPE_POST_CHIMNEY_RECV_REQUEST_COMPLETE: u32 = 119;
pub const MESSAGE2_TYPE_ALLOCATE_RECEIVE_BUFFER_DEPRECATED: u32 = 120;
pub const MESSAGE2_TYPE_ALLOCATE_RECEIVE_BUFFER_COMPLETE_DEPRECATED: u32 = 121;
pub const MESSAGE2_TYPE_FREE_RECEIVE_BUFFER_DEPRECATED: u32 = 122;
pub const MESSAGE2_SEND_VMQ_RNDIS_PACKET_DEPRECATED: u32 = 123;
pub const MESSAGE2_SEND_VMQ_RNDIS_PACKET_COMPLETE_DEPRECATED: u32 = 124;
pub const MESSAGE2_TYPE_SEND_NDIS_CONFIG: u32 = 125;
pub const MESSAGE2_TYPE_ALLOCATE_CHIMNEY_HANDLE: u32 = 126;
pub const MESSAGE2_TYPE_ALLOCATE_CHIMNEY_HANDLE_COMPLETE: u32 = 127;

//
// The maximum allowed message ID for the v2 protocol.
//
pub const MESSAGE2_MAX: u32 = MESSAGE2_TYPE_ALLOCATE_CHIMNEY_HANDLE_COMPLETE;

//
// Version 4 messages
//
pub const MESSAGE4_TYPE_SEND_VF_ASSOCIATION: u32 = 128;
pub const MESSAGE4_TYPE_SWITCH_DATA_PATH: u32 = 129;

//
// Needed so that Win8 RC+ VMs don't AV when running on a Win8 Beta Host
//
pub const MESSAGE4_TYPE_UPLINK_CONNECT_STATE_DEPRECATED: u32 = 130;

//
// The maximum allowed message ID for the v4 protocol.
//
pub const MESSAGE4_MAX: u32 = MESSAGE4_TYPE_UPLINK_CONNECT_STATE_DEPRECATED;

//
// Version 5 messages
//
pub const MESSAGE5_TYPE_OID_QUERY_EX: u32 = 131;
pub const MESSAGE5_TYPE_OID_QUERY_EX_COMPLETE: u32 = 132;
pub const MESSAGE5_TYPE_SUB_CHANNEL: u32 = 133;
pub const MESSAGE5_TYPE_SEND_INDIRECTION_TABLE: u32 = 134;

//
// The maximum allowed message ID for the v5 protocol.
//
pub const MESSAGE5_MAX: u32 = MESSAGE5_TYPE_SEND_INDIRECTION_TABLE;

//
// Version 6 messages
//
pub const MESSAGE6_TYPE_PD_API: u32 = 135;
pub const MESSAGE6_TYPE_PD_POST_BATCH: u32 = 136;
//
// The maximum allowed message ID for the v6 protocol.
//
pub const MESSAGE6_MAX: u32 = MESSAGE6_TYPE_PD_POST_BATCH;

/*
#define NVSP_PROTOCOL_VERSION_1_HANDLER_COUNT \
    ((NvspMessage1Max  - NvspVersionMessageStart) + 1)

#define NVSP_PROTOCOL_VERSION_2_HANDLER_COUNT \
    ((NvspMessage2Max  - NvspVersionMessageStart) + 1)

#define NVSP_PROTOCOL_VERSION_4_HANDLER_COUNT \
    ((NvspMessage4Max  - NvspVersionMessageStart) + 1)

#define NVSP_PROTOCOL_VERSION_5_HANDLER_COUNT \
    ((NvspMessage5Max  - NvspVersionMessageStart) + 1)

//
// Unfortunately, KDNET MiniVSC took a dependency on buggy version 6 of protocol
// (which has number of messages as in protocol version 5). Since all VMs with
// kdnet debugger are be out there, we must handle this version as well.
//
#define NVSP_PROTOCOL_VERSION_6_HANDLER_COUNT \
    ((NvspMessage5Max  - NvspVersionMessageStart) + 1)

#define NVSP_PROTOCOL_VERSION_61_HANDLER_COUNT \
    ((NvspMessage6Max  - NvspVersionMessageStart) + 1)
*/

open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Status: u32 {
        NONE = 0,
        SUCCESS = 1,
        FAILURE = 2,
        INVALID_RNDIS_PACKET = 5,
        BUSY = 6,
        PROTOCOL_VERSION_UNSUPPORTED = 7, // not actually used
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageHeader {
    pub message_type: u32,
}

//
// Init Messages
//

//
// This message is used by the VSC to initialize the channel
// after the channels has been opened. This message should
// never include anything other then versioning (i.e. this
// message will be the same for ever).
//
// For ever is a long time.  The values have been redefined
// in Win7 to indicate major and minor protocol version
// number.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageInit {
    pub protocol_version: u32,  // was MinProtocolVersion
    pub protocol_version2: u32, // was MaxProtocolVersion
}

//
// This message is used by the VSP to complete the initialization
// of the channel. This message should never include anything other
// then versioning (i.e. this message will be the same for ever).
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageInitComplete {
    pub deprecated: u32, // was NegotiatedProtocolVersion (2) in Win6
    pub maximum_mdl_chain_length: u32,
    pub status: Status,
}

pub const INVALID_PROTOCOL_VERSION: u32 = 0xffffffff;

//
// Version 1 Messages
//

//
// This message is used by the VSC to send the NDIS version
// to the VSP. The VSP can use this information when handling
// OIDs sent by the VSC.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendNdisVersion {
    pub ndis_major_version: u32,
    pub ndis_minor_version: u32,
}

//
// This message is used by the VSC to send a receive buffer
// to the VSP. The VSP can then use the receive buffer to
// send data to the VSC.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendReceiveBuffer {
    pub gpadl_handle: GpadlId,
    pub id: u16,
    pub reserved: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReceiveBufferSection {
    pub offset: u32,
    pub sub_allocation_size: u32,
    pub num_sub_allocations: u32,
    pub end_offset: u32,
}

//
// This message is used by the VSP to acknowledge a receive
// buffer send by the VSC. This message must be sent by the
// VSP before the VSP uses the receive buffer.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendReceiveBufferComplete {
    pub status: Status,
    pub num_sections: u32,
    pub sections: [ReceiveBufferSection; 1], // no VSP has ever sent more than 1 section
}

//
// This message is sent by the VSC to revoke the receive buffer.
// After the VSP completes this transaction, the vsp should never
// use the receive buffer again.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1RevokeReceiveBuffer {
    pub id: u16,
}

//
// This message is used by the VSC to send a send buffer
// to the VSP. The VSC can then use the send buffer to
// send data to the VSP.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendSendBuffer {
    pub gpadl_handle: GpadlId,
    pub id: u16,
    pub reserved: u16,
}

//
// This message is used by the VSP to acknowledge a send
// buffer sent by the VSC. This message must be sent by the
// VSP before the VSP uses the sent buffer.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendSendBufferComplete {
    pub status: Status,

    //
    // The VSC gets to choose the size of the send buffer and
    // the VSP gets to choose the sections size of the buffer.
    // This was done to enable dynamic reconfigurations when
    // the cost of GPA-direct buffers decreases.
    //
    pub section_size: u32,
}

//
// This message is sent by the VSC to revoke the send buffer.
// After the VSP completes this transaction, the vsp should never
// use the send buffer again.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1RevokeSendBuffer {
    pub id: u16,
}
//
// This message is used by both the VSP and the VSC to send
// a RNDIS message to the opposite channel endpoint.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendRndisPacket {
    //
    // This field is specified by RNIDS. They assume there's
    // two different channels of communication. However,
    // the Network VSP only has one. Therefore, the channel
    // travels with the RNDIS packet.
    //
    pub channel_type: u32,

    //
    // This field is used to send part or all of the data
    // through a send buffer. This values specifies an
    // index into the send buffer. If the index is
    // 0xFFFFFFFF, then the send buffer is not being used
    // and all of the data was sent through other VMBus
    // mechanisms.
    //
    pub send_buffer_section_index: u32,
    pub send_buffer_section_size: u32,
}

//
// This message is used by both the VSP and the VSC to complete
// a RNDIS message to the opposite channel endpoint. At this
// point, the initiator of this message cannot use any resources
// associated with the original RNDIS packet.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message1SendRndisPacketComplete {
    pub status: Status,
}

//
// This message is used by the VSC to send the NDIS version
// to the VSP. The VSP can use this information when handling
// OIDs sent by the VSC.
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message2SendNdisConfig {
    pub mtu: u32,
    pub reserved: u32,
    pub capabilities: NdisConfigCapabilities,
}

#[derive(Inspect)]
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NdisConfigCapabilities {
    #[inspect(safe)]
    pub vmq: bool,
    #[inspect(safe)]
    pub chimney: bool,
    #[inspect(safe)]
    pub sriov: bool,
    #[inspect(safe)]
    pub ieee_8021_q: bool,
    #[inspect(safe)]
    pub correlation_id: bool,
    #[inspect(safe)]
    pub teaming: bool,
    #[inspect(safe)]
    pub virtual_subnet_id: bool,
    #[inspect(safe)]
    pub rsc_over_vmbus: bool,
    #[bits(56)]
    _reserved: u64,
}

//
// NvspMessage4TypeSendVFAssociation
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message4SendVfAssociation {
    /// Specifies whether VF is allocated for this channel
    /// If 1, SerialNumber of the VF is specified.
    /// If 0, ignore SerialNumber
    pub vf_allocated: u32,

    /// Serial number of the VF to team with
    pub serial_number: u32,
}

//
// This enum is used in specifying the active data path
// in NVSP_4_MESSAGE_SWITCH_DATA_PATH structure
//
open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum DataPath: u32 {
        SYNTHETIC = 0,
        VF = 1,
    }
}

//
// NvspMessage4TypeSwitchDataPath
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message4SwitchDataPath {
    /// Specifies the current data path that is active in the VM
    pub active_data_path: u32,
}

//
// NvspMessage5TypeOidQueryEx
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message5OidQueryEx {
    /// Header information for the Query OID
    header: rndisprot::NdisObjectHeader,
    /// OID being queried
    pub oid: rndisprot::Oid,
}

//
// NvspMessage5TypeOidQueryExComplete
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message5OidQueryExComplete {
    pub status: u32,
    pub bytes: u32,
}

//
// This defines the subchannel requests we can send to the host. We don't need
// the deallocate operation here as when the primary channel closes, the
// subchannels will be closed and we are cleaning up them based on their
// primary channel's channel close callback.
//
open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SubchannelOperation: u32 {
        NONE = 0,
        ALLOCATE = 1,
    }
}

//
// NvspMessage5TypeSubChannel
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message5SubchannelRequest {
    /// The subchannel operation
    pub operation: SubchannelOperation,

    /// The number of subchannels to create, if it is a NvspSubchannelAllocate
    /// operation.
    pub num_sub_channels: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message5SubchannelComplete {
    /// The status of the subchannel operation.
    pub status: Status,

    /// The actual number of subchannels allocated.
    pub num_sub_channels: u32,
}

//
// NvspMessage5TypeSendIndirectionTable
//
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Message5SendIndirectionTable {
    //
    // The number of entries in the send indirection table.
    //
    pub table_entry_count: u32,

    //
    // The offset of the send indireciton table.
    // The send indirection table tells which channel to put the send traffic
    // on. Each entry is a channel number.
    //
    pub table_offset: u32,
}

/*

//
// NvspMessage6TypePdApi
//
typedef enum _NVSP_6_MESSAGE_PD_API_OPERATION
{
    PdApiOpConfiguration = 1,
    PdApiOpSwitchDatapath,
    PdApiOpOpenProvider,
    PdApiOpCloseProvider,
    PdApiOpCreateQueue,
    PdApiOpFlushQueue,
    PdApiOpFreeQueue,
    PdApiOpAllocateCommonBuffer,
    PdApiOpFreeCommonBuffer,
    PdApiOpMax
} NVSP_6_MESSAGE_PD_API_OPERATION;

typedef struct _NVSP_6_MESSAGE_PD_API_REQUEST
{
    UINT32 Operation;
    union
    {
        struct
        {
            //
            // MMIO information is sent from the VM to VSP.
            //
            PHYSICAL_ADDRESS MmioPhysicalAddress;
            UINT32 MmioLength;

            //
            // This is a hint to NVSP: how many PD queues a VM can support.
            //
            UINT16 NumPdQueues;
        } Configuration;

        struct
        {
            BOOLEAN HostDatapathIsPacketDirect;
            BOOLEAN GuestPacketDirectIsEnabled;
        } SwitchDatapath;

        struct
        {
            UINT32 ProviderId;

            //
            // This are the flags from OPEN_PROVIDER structure.
            //
            ULONG Flags;
        } OpenProvider;

        struct
        {
            UINT32 ProviderId;
        } CloseProvider;

        struct
        {
            UINT32 ProviderId;
            UINT16 QueueId;
            UINT16 QueueSize;
            UINT8 IsReceiveQueue;
            UINT8 IsRssQueue;
            UINT32 ReceiveDataLength;
            GROUP_AFFINITY Affinity;
        } CreateQueue;

        struct
        {
            UINT32 ProviderId;
            UINT16 QueueId;
        } DeleteQueue;

        struct
        {
            UINT32 ProviderId;
            UINT16 QueueId;
        } FlushQueue;

        struct
        {
            UINT32 Length;
            UINT32 PreferredNode;
            UINT16 RegionId;
        } AllocateCommonBuffer;

        struct
        {
            UINT32 Length;
            UINT64 PhysicalAddress;
            UINT32 PreferredNode;
            UINT16 RegionId;
            UINT8 CacheType;
        } FreeCommonBuffer;
    };
} NVSP_6_MESSAGE_PD_API_REQUEST, *PNVSP_6_MESSAGE_PD_API_REQUEST;

typedef struct _NVSP_6_MESSAGE_PD_API_COMPLETE
{
    UINT32 Operation;

    //
    // The status of the PD operation in NT STATUS code
    //
    UINT32 Status;

    //
    // Operation specific completion data.
    //
    union
    {
        struct
        {
            //
            // This is the actual number of PD queues allocated to the VM.
            //
            UINT16 NumPdQueues;
            UINT8 NumReceiveRssPDQueues;
            UINT8 IsSupportedByVSP;
            UINT8 IsEnabledByVSP;
        } Configuration;

        struct
        {
            UINT32 ProviderId;
        } OpenProvider;

        struct
        {
            UINT32 ProviderId;
            UINT16 QueueId;
            UINT16 QueueSize;
            UINT32 ReceiveDataLength;
            GROUP_AFFINITY Affinity;
        } CreateQueue;

        struct
        {
            UINT64 PhysicalAddress;
            UINT32 Length;
            UINT32 PreferredNode;
            UINT16 RegionId;
            UINT8 CacheType;
        } AllocateCommonBuffer;

    };
} NVSP_6_MESSAGE_PD_API_COMPLETE, *PNVSP_6_MESSAGE_PD_API_COMPLETE;

typedef struct _NVSP_6_PD_BUFFER
{
    UINT32 RegionOffset;
    UINT16 RegionId;
    UINT16 IsPartial : 1;
    UINT16 ReservedMbz : 15;
} NVSP_6_PD_BUFFER;
C_ASSERT(sizeof(NVSP_6_PD_BUFFER) == sizeof(UINT64));

#pragma warning(disable : 4200)
typedef struct _NVSP_6_MESSAGE_PD_BATCH_MESSAGE
{
    NVSP_MESSAGE_HEADER Header; // Type == NvspMessage6TypePdPostBatch
    UINT16 Count;
    UINT16 GuestToHost : 1;
    UINT16 IsReceive : 1;
    UINT16 ReservedMbz : 14;
    NVSP_6_PD_BUFFER PdBuffer[0];
} NVSP_6_MESSAGE_PD_BATCH_MESSAGE, *PNVSP_6_MESSAGE_PD_BATCH_MESSAGE;

C_ASSERT(sizeof(NVSP_6_MESSAGE_PD_BATCH_MESSAGE) == sizeof(UINT64));

#if defined(VMS_NVIO_EXPERIMENTAL_ENABLED)
//
// Request from the VSC to switch over to NvIo protocol.
// VSP can reject the request.
//
typedef struct _NVSP_7_MESSAGE_USE_NVIO_REQUEST
{
    NVSP_MESSAGE_HEADER Header; // Type == NvspMessage7TypeUseNvIo
    UINT32 ReservedMbz1;
    UINT64 ReservedMbz2;
} NVSP_7_MESSAGE_USE_NVIO_REQUEST, *PNVSP_7_MESSAGE_USE_NVIO_REQUEST;

typedef struct _NVSP_7_MESSAGE_USE_NVIO_COMPLETE
{
    UINT32 Status;
    UINT32 ControlChannelIndex;
} NVSP_7_MESSAGE_USE_NVIO_COMPLETE, *PNVSP_7_MESSAGE_USE_NVIO_COMPLETE;
#endif

//
// NVSP Messages
//
typedef union _NVSP_MESSAGE_1_UBER
{
    NVSP_1_MESSAGE_SEND_NDIS_VERSION            SendNdisVersion;

    NVSP_1_MESSAGE_SEND_RECEIVE_BUFFER          SendReceiveBuffer;
    NVSP_1_MESSAGE_SEND_RECEIVE_BUFFER_COMPLETE SendReceiveBufferComplete;
    NVSP_1_MESSAGE_REVOKE_RECEIVE_BUFFER        RevokeReceiveBuffer;

    NVSP_1_MESSAGE_SEND_SEND_BUFFER             SendSendBuffer;
    NVSP_1_MESSAGE_SEND_SEND_BUFFER_COMPLETE    SendSendBufferComplete;
    NVSP_1_MESSAGE_REVOKE_SEND_BUFFER           RevokeSendBuffer;

    NVSP_1_MESSAGE_SEND_RNDIS_PACKET            SendRNDISPacket;
    NVSP_1_MESSAGE_SEND_RNDIS_PACKET_COMPLETE   SendRNDISPacketComplete;

} NVSP_1_MESSAGE_UBER;

typedef union _NVSP_MESSAGE_2_UBER
{
    NVSP_2_MESSAGE_SEND_NDIS_CONFIG SendNdisConfig;

} NVSP_2_MESSAGE_UBER;

typedef union _NVSP_MESSAGE_4_UBER
{
    NVSP_4_MESSAGE_SEND_VF_ASSOCIATION          VFAssociation;
    NVSP_4_MESSAGE_SWITCH_DATA_PATH             SwitchDataPath;
} NVSP_4_MESSAGE_UBER;

typedef union _NVSP_MESSAGE_5_UBER
{
    NVSP_5_MESSAGE_OID_QUERY_EX                 OidQueryEx;
    NVSP_5_MESSAGE_OID_QUERY_EX_COMPLETE        OidQueryExComplete;
    NVSP_5_MESSAGE_SUBCHANNEL_REQUEST           SubChannelRequest;
    NVSP_5_MESSAGE_SUBCHANNEL_COMPLETE          SubChannelRequestComplete;
    NVSP_5_MESSAGE_SEND_INDIRECTION_TABLE       SendTable;
} NVSP_5_MESSAGE_UBER;

typedef union _NVSP_MESSAGE_6_UBER
{
    NVSP_6_MESSAGE_PD_API_REQUEST               PdApiRequest;
    NVSP_6_MESSAGE_PD_API_COMPLETE              PdApiComplete;
} NVSP_6_MESSAGE_UBER;

#if defined(VMS_NVIO_EXPERIMENTAL_ENABLED)
typedef union _NVSP_MESSAGE_7_UBER
{
    NVSP_7_MESSAGE_USE_NVIO_REQUEST             UseNvIoRequest;
    NVSP_7_MESSAGE_USE_NVIO_COMPLETE            UseNvIoComplete;
} NVSP_7_MESSAGE_UBER;
#endif

typedef union _NVSP_ALL_MESSAGES
{
    NVSP_MESSAGE_INIT_UBER                  InitMessages;
    NVSP_1_MESSAGE_UBER                     Version1Messages;
    NVSP_2_MESSAGE_UBER                     Version2Messages;
    NVSP_4_MESSAGE_UBER                     Version4Messages;
    NVSP_5_MESSAGE_UBER                     Version5Messages;
    NVSP_6_MESSAGE_UBER                     Version6Messages;
#if defined(VMS_NVIO_EXPERIMENTAL_ENABLED)
    NVSP_7_MESSAGE_UBER                     Version7Messages;
#endif
} NVSP_ALL_MESSAGES;

//
// ALL Messages
//
typedef struct _NVSP_MESSAGE
{
    NVSP_MESSAGE_HEADER                     Header;
    NVSP_ALL_MESSAGES                       Messages;
} NVSP_MESSAGE, *PNVSP_MESSAGE;

//
// Message of the protocol version 1 is the biggest of all the legacy messages.
//
#define NVSP_LEGACY_MESSAGE_SIZE    (sizeof(NVSP_MESSAGE_HEADER) + \
                                     sizeof(NVSP_1_MESSAGE_UBER))

//
// Version 6.1 of protocol is the first one that increases the message size.
//
#define NVSP_61_MESSAGE_SIZE        max(NVSP_LEGACY_MESSAGE_SIZE,       \
                                        sizeof(NVSP_MESSAGE_HEADER) +   \
                                        sizeof(NVSP_6_MESSAGE_UBER))

C_ASSERT(NVSP_61_MESSAGE_SIZE > NVSP_LEGACY_MESSAGE_SIZE);

#if defined(VMS_NVIO_EXPERIMENTAL_ENABLED)
//
// Version 7 of protocol
//
#define NVSP_7_MESSAGE_SIZE        max(NVSP_61_MESSAGE_SIZE,            \
                                       sizeof(NVSP_MESSAGE_HEADER) +    \
                                       sizeof(NVSP_7_MESSAGE_UBER))

C_ASSERT(NVSP_7_MESSAGE_SIZE == NVSP_61_MESSAGE_SIZE);
#endif

typedef struct _NVSP_SEND_INDIRECTION_TABLE_MESSAGE
{
    NVSP_MESSAGE NvspMessage;
    UINT32 SendIndirectionTable[VMS_SWITCH_RSS_MAX_SEND_INDIRECTION_TABLE_ENTRIES];
} NVSP_SEND_INDIRECTION_TABLE_MESSAGE, *PNVSP_SEND_INDIRECTION_TABLE_MESSAGE;

//
// The indirection table message is the largest message we send right now without
// using an external MDL. VMBUS requires us to specify the max packet size using
// VmbChannelInitSetMaximumPacketSize. We will not be able to receive packets
// that are larger than this.
//
#define NVSP_MAX_VMBUS_MESSAGE_SIZE (sizeof(NVSP_SEND_INDIRECTION_TABLE_MESSAGE))

//
// Ensure the send indirection table size is equal to 16. This defines the
// legacy NVSP message size (which cannot be changed).
//
// Increasing VMS_SWITCH_RSS_MAX_SEND_INDIRECTION_TABLE_ENTRIES will also increase the
// size of NVSP_SEND_INDIRECTION_TABLE_MESSAGE, which is the largest message we
// currently send without using an external MDL.
//
C_ASSERT(VMS_SWITCH_RSS_MAX_SEND_INDIRECTION_TABLE_ENTRIES == 16);

#pragma pack(pop)
*/
