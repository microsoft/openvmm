// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol for shutdown IC.

use crate::Version;
use bitfield_struct::bitfield;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The unique vmbus interface ID of the shutdown IC.
pub const INTERFACE_ID: Guid = guid::guid!("0e0b6031-5213-4934-818b-38d90ced39db");
/// The unique vmbus instance ID of the shutdown IC.
pub const INSTANCE_ID: Guid = guid::guid!("b6650ff7-33bc-4840-8048-e0676786f393");

/// Supported framework versions.
pub const FRAMEWORK_VERSIONS: &[Version] = &[Version::new(1, 0), Version::new(3, 0)];

/// Supported message versions.
pub const SHUTDOWN_VERSIONS: &[Version] = &[
    Version::new(1, 0),
    Version::new(3, 0),
    Version::new(3, 1),
    Version::new(3, 2),
];

/// The message for shutdown initiated from the host.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ShutdownMessage {
    /// The shutdown reason.
    pub reason_code: u32,
    /// The maximum amount of time allotted to the guest to perform the
    /// shutdown.
    pub timeout_secs: u32,
    /// Flags for the shutdown request.
    pub flags: ShutdownFlags,
    /// Friendly text string for the shutdown request.
    pub message: [u8; 2048],
}

/// Flags for shutdown.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ShutdownFlags {
    /// Whether the shutdown operation is being forced.
    pub force: bool,
    /// Flag indicating the shutdown behavior is guest restart.
    pub restart: bool,
    /// Flag indicating the shutdown behavior is guest hibernate.
    pub hibernate: bool,
    /// Reserved -- must be zero.
    #[bits(29)]
    _reserved: u32,
}

/// Reason code for '[ShutdownMessage]', from Windows SDK.
pub const SHTDN_REASON_FLAG_PLANNED: u32 = 0x80000000;
