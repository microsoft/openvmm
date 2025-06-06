// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants defined by Project Mu's Advanced Logger package,
//! used in the Hyper-V UEFI firmware.

#![warn(missing_docs)]

use crate::uefi::time::EFI_TIME;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

/// Phase value for SEC
pub const SEC_PHASE: u16 = 0x0001;

/// Phase value for PEI CORE
pub const PEI_CORE_PHASE: u16 = 0x0002;

/// Phase value for PEI64
pub const PEI64_PHASE: u16 = 0x0003;

/// Phase value for DXE
pub const DXE_PHASE: u16 = 0x0004;

/// Phase value for RUNTIME
pub const RUNTIME_PHASE: u16 = 0x0005;

/// Phase value for MM CORE
pub const MM_CORE_PHASE: u16 = 0x0006;

/// Phase value for MM
pub const MM_PHASE: u16 = 0x0007;

/// Phase value for SMM CORE
pub const SMM_CORE_PHASE: u16 = 0x0008;

/// Maps phase values to their descriptive names
pub const PHASE_NAMES: &[(u16, &str)] = &[
    (SEC_PHASE, "SEC"),
    (PEI_CORE_PHASE, "PEI_CORE"),
    (PEI64_PHASE, "PEI64"),
    (DXE_PHASE, "DXE"),
    (RUNTIME_PHASE, "RUNTIME"),
    (MM_CORE_PHASE, "MM_CORE"),
    (MM_PHASE, "MM"),
    (SMM_CORE_PHASE, "SMM_CORE"),
];

/// Advanced Logger Info signature
pub const SIG_HEADER: [u8; 4] = *b"ALOG";

/// Advanced Logger Entry signature
pub const SIG_ENTRY: [u8; 4] = *b"ALM2";

/// UEFI Advanced Logger Info Header, which is shared
/// with the Advanced Logger Package in UEFI. The entries
/// live right after.
#[repr(C)]
#[derive(Debug, Copy, Clone, FromBytes, Immutable, KnownLayout)]
pub struct AdvancedLoggerInfo {
    /// Signature 'ALOG'
    pub signature: u32,
    /// Current Version
    pub version: u16,
    /// Reserved for future
    pub reserved: [u16; 3],
    /// Offset from LoggerInfo to start of log
    pub log_buffer_offset: u32,
    /// Reserved field
    pub reserved4: u32,
    /// Offset from LoggerInfo to where to store next log entry
    pub log_current_offset: u32,
    /// Number of bytes of messages missed
    pub discarded_size: u32,
    /// Size of allocated buffer
    pub log_buffer_size: u32,
    /// Log in permanent RAM
    pub in_permanent_ram: u8,
    /// After ExitBootServices
    pub at_runtime: u8,
    /// After VirtualAddressChange
    pub gone_virtual: u8,
    /// HdwPort initialized
    pub hdw_port_initialized: u8,
    /// HdwPort is Disabled
    pub hdw_port_disabled: u8,
    /// Reserved field
    pub reserved2: [u8; 3],
    /// Ticks per second for log timing
    pub timer_frequency: u64,
    /// Ticks when Time Acquired
    pub ticks_at_time: u64,
    /// Uefi Time Field
    pub time: EFI_TIME,
    /// Logging level to be printed at hw port
    pub hw_print_level: u32,
    /// Reserved field
    pub reserved3: u32,
}

/// UEFI Advanced Logger Entry, which is shared with the
/// Advanced Logger Package in UEFI. The messages live
/// right after.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone, FromBytes, Immutable, KnownLayout)]
pub struct AdvancedLoggerMessageEntryV2 {
    /// Signature 'ALM2'
    pub signature: u32,
    /// Major version of the advanced logger message structure.
    pub major_version: u8,
    /// Minor version of the advanced logger message structure.
    pub minor_version: u8,
    /// Debug level
    pub debug_level: u32,
    /// Time stamp
    pub time_stamp: u64,
    /// Boot phase that produced this message entry
    pub phase: u16,
    /// Number of bytes in the Message
    pub message_len: u16,
    /// Offset of the message from the start of the structure.
    pub message_offset: u16,
    // Rust prevents C flexible array members, but "message_text: [u8; _]" would be here
}
