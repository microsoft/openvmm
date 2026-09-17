// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio vsock device specification constants and types.
//!
//! Based on OASIS VIRTIO v1.3, Section 5.10.
//! <https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html>

#![expect(
    dead_code,
    reason = "This module defines constants and types for the virtio-vsock spec, but not all of them are used in our implementation."
)]

use bitfield_struct::bitfield;
use open_enum::open_enum;
use std::io::IoSlice;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[bitfield(u32)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct VsockFeaturesBank0 {
    pub stream: bool,
    pub seqpacket: bool,
    pub no_implied_stream: bool,
    #[bits(29)]
    _reserved: u32,
}

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

impl VsockHeader {
    /// Gets the operation specified in this header.
    pub fn operation(&self) -> Operation {
        Operation(self.op)
    }

    pub fn socket_type(&self) -> SocketType {
        SocketType(self.socket_type)
    }

    pub fn shutdown_flags(&self) -> ShutdownFlags {
        ShutdownFlags::from_bits(self.flags)
    }
}

pub const VSOCK_HEADER_SIZE: usize = size_of::<VsockHeader>();

pub struct VsockPacket<'a> {
    pub header: VsockHeader,
    pub data: &'a [IoSlice<'a>],
}

impl<'a> VsockPacket<'a> {
    pub fn new(header: VsockHeader, data: &'a [IoSlice<'a>]) -> Self {
        Self { header, data }
    }
}

pub struct VsockPacketBuf {
    pub header: VsockHeader,
    pub data: Vec<u8>,
}

impl VsockPacketBuf {
    pub fn new(header: VsockHeader, data: Vec<u8>) -> Self {
        Self { header, data }
    }

    pub fn header_only(header: VsockHeader) -> Self {
        Self {
            header,
            data: Vec::new(),
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

open_enum! {
    /// Event IDs for the event virtqueue.
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub enum Event: u16 {
        TRANSPORT_RESET = 0,
    }
}

/// Event structure sent on the event virtqueue.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct VsockEvent {
    pub id: u32,
}
