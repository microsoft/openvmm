// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dispatcher for isolation-specific initialization functions

#[cfg(target_arch = "x86_64")]
use crate::get_tdx_tsc_reftime;

/// Isolation type of the partition
///
/// TODO: Fix arch specific abstractions across the bootloader so we can remove
/// target_arch here and elsewhere.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum IsolationType {
    None,
    Vbs,
    #[cfg(target_arch = "x86_64")]
    Snp,
    #[cfg(target_arch = "x86_64")]
    Tdx,
}

impl IsolationType {
    pub fn get_ref_time(&self) -> Option<u64> {
        match self {
            #[cfg(target_arch = "x86_64")]
            IsolationType::Tdx => get_tdx_tsc_reftime(),
            #[cfg(target_arch = "x86_64")]
            IsolationType::Snp => None,
            _ => Some(minimal_rt::reftime::reference_time()),
        }
    }

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
