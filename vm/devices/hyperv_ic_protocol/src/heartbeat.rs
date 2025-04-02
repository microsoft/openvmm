// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Heartbeat component protocol.
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Heartbeat message from guest to host.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HeartbeatMessage {
    /// Incrementing sequence counter.
    pub sequence_number: u64,
    /// Current state of the guest.
    pub application_state: ApplicationState,
    /// Reserved.
    pub reserved: [u8; 4],
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    /// Current state of guest.
    pub enum ApplicationState: u32 {
        /// Guest is in an unknown state.
        UNKNOWN = 0,
        /// Guest is healthy.
        HEALTHY = 1,
        /// Guest encountered a critical error.
        CRITICAL = 2,
        /// Guest is no longer running.
        STOPPED = 3,
    }
}
