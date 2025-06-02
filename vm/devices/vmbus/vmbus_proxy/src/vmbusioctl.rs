// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::upper_case_acronyms
)]

use bitfield_struct::bitfield;
use vmbus_core::protocol::UserDefinedData;
use windows::core::GUID;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VMBUS_CHANNEL_OFFER {
    pub InterfaceType: GUID,
    pub InterfaceInstance: GUID,
    pub InterruptLatencyIn100nsUnits: u64,
    pub ChannelFlags: VmbusChannelOfferFlags,
    pub MmioMegabytes: u16,         // in bytes * 1024 * 1024
    pub MmioMegabytesOptional: u16, // mmio memory in addition to MmioMegabytes that is optional
    pub SubChannelIndex: u16,
    pub TargetVtl: u8,
    pub Reserved: [u8; 7],
    pub UserDefined: UserDefinedData,
}

#[bitfield(u16)]
pub struct VmbusChannelOfferFlags {
    pub enumerate_device_interface: bool, // 0x1
    #[bits(3)]
    _reserved1: u16,
    pub named_pipe_mode: bool, // 0x10
    #[bits(5)]
    _reserved2: u16,
    pub request_monitored_notification: bool, // 0x400
    pub _reserved3: bool,
    pub force_new_channel: bool, // 0x1000
    pub tlnpi_provider: bool,    // 0x2000
    #[bits(2)]
    _reserved4: u16,
}

#[repr(C)]
#[derive(Copy, Clone, zerocopy::IntoBytes)]
pub struct VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
    pub RingBufferGpadlHandle: u32,
    pub DownstreamRingBufferPageOffset: u32,
    pub NodeNumber: u16,
    pub Padding: u16,
}
