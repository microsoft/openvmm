// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

//! Isolation type definition.

/// Isolation type of the partition
///
/// TODO: Fix arch specific abstractions across the bootloader so we can remove
/// target_arch here and elsewhere.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum IsolationType {
    /// No isolation is in use by this guest.
    None,
    /// This guest is isolated with VBS.
    Vbs,
    /// This guest is isolated with SNP (physical or emulated).
    #[cfg(target_arch = "x86_64")]
    Snp,
    /// This guest is isolated with TDX (physical or emulated).
    #[cfg(target_arch = "x86_64")]
    Tdx,
}

impl IsolationType {
    /// Returns true if this partition is hardware isolcated ie SNP and TDX today
    pub fn is_hardware_isolated(&self) -> bool {
        match self {
            IsolationType::None => false,
            IsolationType::Vbs => false,
            #[cfg(target_arch = "x86_64")]
            IsolationType::Snp => true,
            #[cfg(target_arch = "x86_64")]
            IsolationType::Tdx => true,
        }
    }
}
