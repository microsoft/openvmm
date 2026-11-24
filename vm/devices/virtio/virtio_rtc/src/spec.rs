// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! constants and type definitions for the Virtio 1.4 RTC device.

use virtio::spec::u16_le;
use virtio::spec::u64_le;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const REQ_READ: u16 = 0x0001;
pub const REQ_READ_CROSS: u16 = 0x0002;
pub const REQ_CFG: u16 = 0x1000;
pub const REQ_CLOCK_CAP: u16 = 0x1001;
pub const REQ_CROSS_CAP: u16 = 0x1002;
pub const REQ_READ_ALARM: u16 = 0x1003;
pub const REQ_SET_ALARM: u16 = 0x1004;
pub const REQ_SET_ALARM_ENABLED: u16 = 0x1005;

pub const S_OK: u8 = 0;
pub const S_EOPNOTSUPP: u8 = 2;
pub const S_ENODEV: u8 = 3;
pub const S_EINVAL: u8 = 4;
pub const S_EIO: u8 = 5;

pub const CLOCK_UTC_MAYBE_SMEARED: u8 = 4;
pub const SMEAR_UNSPECIFIED: u8 = 0;

pub const COUNTER_ARM_VCT: u8 = 0;
pub const COUNTER_X86_TSC: u8 = 1;
pub const COUNTER_INVALID: u8 = 0xff;

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ReqHead {
    pub msg_type: u16_le,
    pub reserved: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespHead {
    pub status: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ReqClockBody {
    pub clock_id: u16_le,
    pub reserved: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ReqCrossBody {
    pub clock_id: u16_le,
    pub hw_counter: u8,
    pub reserved: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ReqSetAlarmBody {
    pub alarm_time: u64_le,
    pub clock_id: u16_le,
    pub flags: u8,
    pub reserved: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct ReqSetAlarmEnabledBody {
    pub clock_id: u16_le,
    pub flags: u8,
    pub reserved: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespCfg {
    pub head: RespHead,
    pub num_clocks: u16_le,
    pub reserved: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespClockCap {
    pub head: RespHead,
    pub clock_type: u8,
    pub leap_second_smearing: u8,
    pub flags: u8,
    pub reserved: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespCrossCap {
    pub head: RespHead,
    pub flags: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespRead {
    pub head: RespHead,
    pub clock_reading: u64_le,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespReadCross {
    pub head: RespHead,
    pub clock_reading: u64_le,
    pub counter_cycles: u64_le,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RespReadAlarm {
    pub head: RespHead,
    pub alarm_time: u64_le,
    pub flags: u8,
    pub reserved: [u8; 7],
}
