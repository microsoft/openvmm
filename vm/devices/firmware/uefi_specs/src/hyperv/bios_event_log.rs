// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants defined in `BiosEventLogInterface.h`

use guid::Guid;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Represents an event channel plus data.
///
/// This is used when flushing a UEFI event channel to the BIOS device.
/// Data is series of EFI_EVENT_DESCRIPTORs with variable sized data.
///
/// reSearch query: `BIOS_EVENT_CHANNEL`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BiosEventChannel {
    pub channel: Guid,
    pub events_written: u32,
    pub events_lost: u32,
    pub data_size: u32,
    // Payload of size `data_size`
}

/// reSearch query: `EFI_EVENT_DESCRIPTOR`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EfiEventDescriptor {
    pub producer: Guid,       // Optional GUID identifying the producer of the event
    pub correlation_id: Guid, // Optional Guid used to correlate an event entry with another event entry
    pub create_time: u64,     // Timestamp when the event was created
    pub commit_time: u64, // Timestamp when the event was committed (may be the same as CreateTime)
    pub event_id: u32,    // Producer specified identifier
    pub flags: u32,       // See EVENT_FLAG_nnnnn
    pub header_size: u32, // Size of this header structure
    pub data_size: u32,   // Associated Data Size
                          // New fields should be added here.
}

const_assert_eq!(size_of::<EfiEventDescriptor>(), 64);
