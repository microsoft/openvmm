// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V specific UEFI Time definitions

use crate::uefi::common::EfiStatus64;
use crate::uefi::time::EFI_TIME;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

/// MsvmPkg: `VM_EFI_TIME``
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, AsBytes, FromBytes, FromZeroes)]
pub struct VmEfiTime {
    pub status: EfiStatus64,
    pub time: EFI_TIME,
}
