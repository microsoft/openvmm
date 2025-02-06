// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::upper_case_acronyms
)]

use vmbus_core::protocol::UserDefinedData;
use windows::core::GUID;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VMBUS_CHANNEL_OFFER {
    pub InterfaceType: GUID,
    pub InterfaceInstance: GUID,
    pub InterruptLatencyIn100nsUnits: u64,
    pub ChannelFlags: u16,
    pub MmioMegabytes: u16,         // in bytes * 1024 * 1024
    pub MmioMegabytesOptional: u16, // mmio memory in addition to MmioMegabytes that is optional
    pub SubChannelIndex: u16,
    pub TargetVtl: u8,
    pub Reserved: [u8; 7],
    pub UserDefined: UserDefinedData,
}

pub const VMBUS_CHANNEL_ENUMERATE_DEVICE_INTERFACE: u16 = 1;
pub const VMBUS_CHANNEL_NAMED_PIPE_MODE: u16 = 0x10;
pub const VMBUS_CHANNEL_LOOPBACK_OFFER: u16 = 0x100;
pub const VMBUS_CHANNEL_REQUEST_MONITORED_NOTIFICATION: u16 = 0x400;
pub const VMBUS_CHANNEL_FORCE_NEW_CHANNEL: u16 = 0x1000;
pub const VMBUS_CHANNEL_TLNPI_PROVIDER_OFFER: u16 = 0x2000;

pub const VMBUS_PIPE_TYPE_BYTE: u32 = 0;
pub const VMBUS_PIPE_TYPE_MESSAGE: u32 = 4;
pub const VMBUS_PIPE_TYPE_RAW: u32 = 8;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
    pub RingBufferGpadlHandle: u32,
    pub DownstreamRingBufferPageOffset: u32,
    pub NodeNumber: u16,
}
