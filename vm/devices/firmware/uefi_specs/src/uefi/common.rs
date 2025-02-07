// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common UEFI spec types.

use core::fmt::Debug;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LittleEndian;
use zerocopy::U64;

open_enum! {
    /// UEFI spec Appendix D - Status Codes
    ///
    /// Note that EFI_STATUS is encoded as a `UINTN` in UEFI, so it is either 32
    /// or 64 bits wide (with the error bit always being the high bit). This
    /// enum is defined as 64 bits wide so that it does not lose any (invalid)
    /// high bits when taking a guest-provided 64-bit value.
    ///
    /// However, this type is not intended for direct sharing with the guest, so
    /// it does not derive `IntoBytes`, etc. To be clear about intent when using
    /// this value for communication with the guest via shared memory, use
    /// [`EfiStatus64`] instead. If you are implementing a legacy protocol that
    /// does not preserve the error bit, use
    /// [`EfiStatus64NoErrorBit`](crate::hyperv::common::EfiStatus64NoErrorBit).
    #[must_use]
    pub enum EfiStatus: u64 {
        SUCCESS =                   0,
        LOAD_ERROR =                1 | Self::ERROR_BIT,
        INVALID_PARAMETER =         2 | Self::ERROR_BIT,
        UNSUPPORTED =               3 | Self::ERROR_BIT,
        BAD_BUFFER_SIZE =           4 | Self::ERROR_BIT,
        BUFFER_TOO_SMALL =          5 | Self::ERROR_BIT,
        NOT_READY =                 6 | Self::ERROR_BIT,
        DEVICE_ERROR =              7 | Self::ERROR_BIT,
        WRITE_PROTECTED =           8 | Self::ERROR_BIT,
        OUT_OF_RESOURCES =          9 | Self::ERROR_BIT,
        VOLUME_CORRUPTED =          10 | Self::ERROR_BIT,
        VOLUME_FULL =               11 | Self::ERROR_BIT,
        NO_MEDIA =                  12 | Self::ERROR_BIT,
        MEDIA_CHANGED =             13 | Self::ERROR_BIT,
        NOT_FOUND =                 14 | Self::ERROR_BIT,
        ACCESS_DENIED =             15 | Self::ERROR_BIT,
        NO_RESPONSE =               16 | Self::ERROR_BIT,
        NO_MAPPING =                17 | Self::ERROR_BIT,
        TIMEOUT =                   18 | Self::ERROR_BIT,
        NOT_STARTED =               19 | Self::ERROR_BIT,
        ALREADY_STARTED =           20 | Self::ERROR_BIT,
        ABORTED =                   21 | Self::ERROR_BIT,
        ICMP_ERROR =                22 | Self::ERROR_BIT,
        TFTP_ERROR =                23 | Self::ERROR_BIT,
        PROTOCOL_ERROR =            24 | Self::ERROR_BIT,
        INCOMPATIBLE_VERSION =      25 | Self::ERROR_BIT,
        SECURITY_VIOLATION =        26 | Self::ERROR_BIT,
        CRC_ERROR =                 27 | Self::ERROR_BIT,
        END_OF_MEDIA =              28 | Self::ERROR_BIT,
        END_OF_FILE =               31 | Self::ERROR_BIT,
        INVALID_LANGUAGE =          32 | Self::ERROR_BIT,
        COMPROMISED_DATA =          33 | Self::ERROR_BIT,
        IP_ADDRESS_CONFLICT =       34 | Self::ERROR_BIT,
        HTTP_ERROR =                35 | Self::ERROR_BIT,
    }
}

impl Default for EfiStatus {
    fn default() -> Self {
        Self::SUCCESS
    }
}

impl EfiStatus {
    pub const ERROR_BIT: u64 = 1 << 63;
}

/// A 64-bit, unaligned, little-endian encoding of [`EfiStatus`], appropriate
/// for sharing with the guest.
#[repr(transparent)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EfiStatus64(pub U64<LittleEndian>);

impl Debug for EfiStatus64 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        EfiStatus::from(*self).fmt(f)
    }
}

impl From<EfiStatus> for EfiStatus64 {
    fn from(value: EfiStatus) -> Self {
        Self(value.0.into())
    }
}

impl From<EfiStatus64> for EfiStatus {
    fn from(value: EfiStatus64) -> Self {
        Self(value.0.get())
    }
}
