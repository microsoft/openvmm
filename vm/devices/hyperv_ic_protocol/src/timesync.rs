// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol definitions for the timesync IC.

use crate::Version;
use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::little_endian::U64 as U64LE;

/// Version 1.0.
pub const TIMESYNC_VERSION_1: Version = Version::new(1, 0);
/// Version 3.0.
pub const TIMESYNC_VERSION_3: Version = Version::new(3, 0);
/// Version 4.0. Introduced a new message format.
pub const TIMESYNC_VERSION_4: Version = Version::new(4, 0);

/// Timesync messages used before version 4.0.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TimesyncMessage {
    /// The time of day measured in the parent, in UTC (or TAI? unclear).
    pub parent_time: U64LE,
    /// Unused.
    pub child_time: U64LE,
    /// The measured round trip time by the parent.
    pub round_trip_time: U64LE,
    /// Flags indicating the message's purpose.
    pub flags: TimesyncFlags,
    /// Reserved.
    pub reserved: [u8; 3],
}

/// Timesync messages used in version 4.0 and later.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TimesyncMessageV4 {
    /// The wall clock time measured in the parent, in UTC (or TAI? unclear).
    pub parent_time: U64LE,
    /// The VM reference time of the child, at the time the parent measured the
    /// wall clock time.
    pub vm_reference_time: u64,
    /// Flags indicating the message's purpose.
    pub flags: TimesyncFlags,
    /// The NTP leap indicator.
    pub leap_indicator: u8,
    /// The NTP stratum.
    pub stratum: u8,
    /// Reserved.
    pub reserved: [u8; 5],
}

/// Flags for timesync messages.
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TimesyncFlags {
    /// This is a sync message.
    pub sync: bool,
    /// This is a sample message.
    pub sample: bool,
    #[bits(6)]
    _rsvd: u8,
}
