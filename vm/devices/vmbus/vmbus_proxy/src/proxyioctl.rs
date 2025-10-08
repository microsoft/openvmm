// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(
    dead_code,
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types
)]

use super::vmbusioctl::VMBUS_CHANNEL_OFFER;
use super::vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use bitfield_struct::bitfield;
use guid::Guid;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::System::Ioctl::FILE_DEVICE_UNKNOWN;
use windows::Win32::System::Ioctl::FILE_READ_ACCESS;
use windows::Win32::System::Ioctl::FILE_WRITE_ACCESS;
use windows::Win32::System::Ioctl::METHOD_BUFFERED;
use windows::core::GUID;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

const fn CTL_CODE(DeviceType: u32, Function: u32, Method: u32, Access: u32) -> u32 {
    (DeviceType << 16) | (Access << 14) | (Function << 2) | Method
}

const fn VMBUS_PROXY_IOCTL(code: u32) -> u32 {
    CTL_CODE(
        FILE_DEVICE_UNKNOWN,
        code,
        METHOD_BUFFERED,
        FILE_READ_ACCESS | FILE_WRITE_ACCESS,
    )
}

pub const IOCTL_VMBUS_PROXY_SET_VM_NAME: u32 = VMBUS_PROXY_IOCTL(0x1);
pub const IOCTL_VMBUS_PROXY_SET_TOPOLOGY: u32 = VMBUS_PROXY_IOCTL(0x2);
pub const IOCTL_VMBUS_PROXY_SET_MEMORY: u32 = VMBUS_PROXY_IOCTL(0x3);
pub const IOCTL_VMBUS_PROXY_NEXT_ACTION: u32 = VMBUS_PROXY_IOCTL(0x4);
pub const IOCTL_VMBUS_PROXY_OPEN_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x5);
pub const IOCTL_VMBUS_PROXY_CLOSE_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x6);
pub const IOCTL_VMBUS_PROXY_CREATE_GPADL: u32 = VMBUS_PROXY_IOCTL(0x7);
pub const IOCTL_VMBUS_PROXY_DELETE_GPADL: u32 = VMBUS_PROXY_IOCTL(0x8);
pub const IOCTL_VMBUS_PROXY_RELEASE_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x9);
pub const IOCTL_VMBUS_PROXY_RUN_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0xa);
pub const IOCTL_VMBUS_PROXY_SET_VID_HANDLE: u32 = VMBUS_PROXY_IOCTL(0xb);
pub const IOCTL_VMBUS_PROXY_TL_CONNECT_REQUEST: u32 = VMBUS_PROXY_IOCTL(0xc);
pub const IOCTL_VMBUS_PROXY_RESTORE_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0xd);
pub const IOCTL_VMBUS_PROXY_REVOKE_UNCLAIMED_CHANNELS: u32 = VMBUS_PROXY_IOCTL(0xe);
pub const IOCTL_VMBUS_PROXY_RESTORE_SET_INTERRUPT: u32 = VMBUS_PROXY_IOCTL(0xf);

#[repr(C)]
#[derive(Copy, Clone, zerocopy::IntoBytes)]
pub struct VMBUS_PROXY_SET_VM_NAME_INPUT {
    pub VmId: [u8; 16],
    pub NameLength: u16,
    pub NameOffset: u16,
}

#[repr(C)]
#[derive(Copy, Clone, zerocopy::IntoBytes)]
pub struct VMBUS_PROXY_SET_TOPOLOGY_INPUT {
    pub NodeCount: u32,
    pub VpCount: u32,
    pub NodesOffset: u32,
}

#[repr(C)]
#[derive(Copy, Clone, zerocopy::IntoBytes)]
pub struct VMBUS_PROXY_SET_MEMORY_INPUT {
    pub BaseAddress: u64,
    pub Size: u64,
}

pub const VmbusProxyActionTypeOffer: u32 = 1;
pub const VmbusProxyActionTypeRevoke: u32 = 2;
pub const VmbusProxyActionTypeInterruptPolicy: u32 = 3;
pub const VmbusProxyActionTypeTlConnectResult: u32 = 4;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_NEXT_ACTION_OUTPUT {
    pub Type: u32,
    pub Padding: u32,
    pub ProxyId: u64,
    pub u: VMBUS_PROXY_NEXT_ACTION_OUTPUT_union,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union VMBUS_PROXY_NEXT_ACTION_OUTPUT_union {
    pub Offer: VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_Offer,
    pub TlConnectResult: VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_TlConnectResult,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_Offer {
    pub Offer: VMBUS_CHANNEL_OFFER,
    pub DeviceIncomingRingEvent: u64, // BUGBUG: HANDLE
    pub DeviceOutgoingRingEvent: u64, // BUGBUG: HANDLE
    pub DeviceOrder: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_TlConnectResult {
    pub EndpointId: GUID,
    pub ServiceId: GUID,
    pub Status: NTSTATUS,
    pub Vtl: u8,
    pub Padding: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_OPEN_CHANNEL_INPUT {
    pub ProxyId: u64,
    pub OpenParameters: VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS,
    pub Padding: u32,
    pub VmmSignalEvent: u64, // BUGBUG: HANDLE
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_SET_INTERRUPT_INPUT {
    pub ProxyId: u64,
    pub VmmSignalEvent: u64, // BUGBUG: HANDLE
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_OPEN_CHANNEL_OUTPUT {
    pub Status: i32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
pub struct VMBUS_PROXY_RESTORE_CHANNEL_INPUT {
    pub InterfaceType: Guid,
    pub InterfaceInstance: Guid,
    pub SubchannelIndex: u16,
    pub TargetVtl: u8,
    pub Open: u8,
    pub OpenParameters: VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS,
    pub GpadlCount: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_RESTORE_CHANNEL_OUTPUT {
    pub ProxyId: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_CLOSE_CHANNEL_INPUT {
    pub ProxyId: u64,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
pub struct VMBUS_PROXY_CREATE_GPADL_INPUT {
    pub ProxyId: u64,
    pub GpadlId: u32,
    pub RangeCount: u32,
    pub RangeBufferOffset: u32,
    pub RangeBufferSize: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_DELETE_GPADL_INPUT {
    pub ProxyId: u64,
    pub GpadlId: u32,
    pub Padding: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_RELEASE_CHANNEL_INPUT {
    pub ProxyId: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_RUN_CHANNEL_INPUT {
    pub ProxyId: u64,
}

#[bitfield(u32)]
pub struct VMBUS_PROXY_TL_CONNECT_REQUEST_FLAGS {
    pub hosted_silo_unaware: bool,
    #[bits(31)]
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VMBUS_PROXY_TL_CONNECT_REQUEST_INPUT {
    pub EndpoindId: GUID,
    pub ServiceId: GUID,
    pub SiloId: GUID,
    pub Flags: VMBUS_PROXY_TL_CONNECT_REQUEST_FLAGS,
    pub Vtl: u8,
    pub Padding: [u8; 3],
}
