// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types shared between multiple parts of the Hyper-V UEFI protocols.

use crate::uefi::common::EfiStatus;
use core::fmt::Debug;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LittleEndian;
use zerocopy::U64;

/// A 64-bit, unaligned, little-endian encoding of [`EfiStatus`] that does not
/// include the error bit.
///
/// This should be used for the Hyper-V NVRAM and crypto protocols only.
///
/// This encoding cannot be round tripped, nor can it be used to return warnings
/// to the guest. UEFI warning values (non-zero status values that do not have
/// the error bit set) will be turned into errors.
///
/// Luckily, such warnings statuses are rare in practice and are unused by
/// Hyper-V UEFI protocols.
#[repr(transparent)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EfiStatus64NoErrorBit(pub U64<LittleEndian>);

impl From<EfiStatus> for EfiStatus64NoErrorBit {
    fn from(value: EfiStatus) -> Self {
        Self((value.0 & !EfiStatus::ERROR_BIT).into())
    }
}

impl From<EfiStatus64NoErrorBit> for EfiStatus {
    fn from(value: EfiStatus64NoErrorBit) -> Self {
        if value.0.get() == 0 {
            Self::SUCCESS
        } else {
            Self(value.0.get() | Self::ERROR_BIT)
        }
    }
}

impl Debug for EfiStatus64NoErrorBit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        EfiStatus::from(*self).fmt(f)
    }
}
