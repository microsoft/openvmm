// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio vsock protocol definitions from the virtio specification, section 5.10.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Virtio device type ID for socket devices.
pub const VIRTIO_DEVICE_TYPE_VSOCK: u16 = 19;

// Feature bits defined by the spec but not all actively used.
#[allow(dead_code)]
/// Feature bit: stream socket type support (always set, mandatory).
pub const VIRTIO_VSOCK_F_STREAM: u32 = 0; // Implicit, no feature bit needed
#[allow(dead_code)]
/// Feature bit: SOCK_SEQPACKET type support (optional).
pub const VIRTIO_VSOCK_F_SEQPACKET: u32 = 1;

#[allow(dead_code)]
/// Well-known CID values.
pub const VSOCK_CID_HYPERVISOR: u64 = 0;
pub const VSOCK_CID_HOST: u64 = 2;

/// Virtio vsock device configuration space.
///
/// The device configuration provides the guest CID.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct VsockConfig {
    /// The guest_cid field contains the guest's context ID.
    pub guest_cid: u64,
}

/// Virtio vsock packet header, prepended to every data packet on the rx/tx
/// virtqueues.
///
/// All fields are little-endian.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct VsockHeader {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    /// The socket type (VIRTIO_VSOCK_TYPE_*).
    pub socket_type: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

/// Socket types for the `type` field.
pub const VIRTIO_VSOCK_TYPE_STREAM: u16 = 1;

/// Operations for the `op` field.
#[allow(dead_code)]
pub const VIRTIO_VSOCK_OP_INVALID: u16 = 0;
pub const VIRTIO_VSOCK_OP_REQUEST: u16 = 1;
pub const VIRTIO_VSOCK_OP_RESPONSE: u16 = 2;
pub const VIRTIO_VSOCK_OP_RST: u16 = 3;
pub const VIRTIO_VSOCK_OP_SHUTDOWN: u16 = 4;
pub const VIRTIO_VSOCK_OP_RW: u16 = 5;
pub const VIRTIO_VSOCK_OP_CREDIT_UPDATE: u16 = 6;
pub const VIRTIO_VSOCK_OP_CREDIT_REQUEST: u16 = 7;

/// Shutdown flags for VIRTIO_VSOCK_OP_SHUTDOWN.
pub const VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE: u32 = 1;
pub const VIRTIO_VSOCK_SHUTDOWN_F_SEND: u32 = 2;

#[allow(dead_code)]
/// Event IDs for the event virtqueue.
pub const VIRTIO_VSOCK_EVENT_TRANSPORT_RESET: u32 = 0;

#[allow(dead_code)]
/// Event structure sent on the event virtqueue.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct VsockEvent {
    pub id: u32,
}

impl VsockHeader {
    /// Create a new header for a packet from the host to the guest.
    pub fn new_reply(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32, op: u16) -> Self {
        Self {
            src_cid: src_cid.to_le(),
            dst_cid: dst_cid.to_le(),
            src_port: src_port.to_le(),
            dst_port: dst_port.to_le(),
            len: 0u32.to_le(),
            socket_type: VIRTIO_VSOCK_TYPE_STREAM.to_le(),
            op: op.to_le(),
            flags: 0u32.to_le(),
            buf_alloc: 0u32.to_le(),
            fwd_cnt: 0u32.to_le(),
        }
    }
}
