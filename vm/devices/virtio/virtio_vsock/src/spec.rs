// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio vsock protocol definitions from the virtio specification, section 5.10.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use std::io::IoSlice;
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
    pub socket_type: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

pub struct VsockPacket<'a> {
    pub header: VsockHeader,
    pub data: &'a [IoSlice<'a>],
    pub data_len: usize,
}

impl<'a> VsockPacket<'a> {
    pub fn new(header: VsockHeader, data: &'a [IoSlice<'a>], data_len: usize) -> Self {
        Self {
            header,
            data,
            data_len,
        }
    }

    pub fn header_only(header: VsockHeader) -> Self {
        Self {
            header,
            data: &[],
            data_len: 0,
        }
    }
}

open_enum! {
    /// Socket types for the `type` field.
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub enum SocketType: u16 {
        STREAM = 1,
    }
}

open_enum! {
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub enum Operation: u16 {
        INVALID = 0,
        REQUEST = 1,
        RESPONSE = 2,
        RST = 3,
        SHUTDOWN = 4,
        RW = 5,
        CREDIT_UPDATE = 6,
        CREDIT_REQUEST = 7,
    }
}

#[bitfield(u32)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct ShutdownFlags {
    pub receive: bool,
    pub send: bool,
    #[bits(30)]
    _reserved: u32,
}

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
