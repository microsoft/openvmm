// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for loading the MSVM UEFI firmware.

use open_enum::open_enum;

open_enum! {
    /// SEC platform type passed by the loader to the firmware in `x2` at SEC
    /// entry on aarch64. Mirrors `MSVM_SEC_PLATFORM_TYPE` in the mu_msvm
    /// firmware (`MsvmPkg/Include/Ppi/SecPlatformType.h`).
    pub enum SecPlatformType: u64 {
        /// Hyper-V with Microsoft extensions.
        HYPERV = 0,
        /// Generic virtualization without Microsoft hypervisor extensions.
        GENERIC = 1,
    }
}
