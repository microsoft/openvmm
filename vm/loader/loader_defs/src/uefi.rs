// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for loading the MSVM UEFI firmware.

use open_enum::open_enum;

open_enum! {
    /// SEC platform type passed by the loader to the firmware in `x2` at SEC
    /// entry on aarch64. Mirrors `MSVM_SEC_PLATFORM_TYPE` in the mu_msvm
    /// firmware (`MsvmPkg/Include/Ppi/SecPlatformType.h`).
    ///
    /// [`SecPlatformType::HYPERV`] (0) is the reset default, so it does not need
    /// to be set explicitly; [`SecPlatformType::GENERIC`] (1) is passed when the
    /// hypervisor (HV#1) enlightenments are not exposed to the guest (e.g.
    /// booting under aarch64 KVM).
    pub enum SecPlatformType: u64 {
        /// Hyper-V with Microsoft extensions.
        HYPERV = 0,
        /// Generic virtualization without Microsoft hypervisor extensions.
        GENERIC = 1,
    }
}
