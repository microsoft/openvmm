// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor selection for the microVM machine profile.

use super::choose_hypervisor;
use hypervisor_resources::HypervisorKind;
use vm_resource::Resource;

/// Returns the native backend supported by the microVM profile.
pub fn choose_microvm_hypervisor() -> anyhow::Result<Resource<HypervisorKind>> {
    #[cfg(any(target_os = "linux", windows))]
    return choose_hypervisor();
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("the microVM profile requires a Linux KVM/MSHV or Windows WHP host");
}
