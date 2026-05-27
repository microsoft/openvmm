// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HVF (macOS Hypervisor.framework) hypervisor backend.

#![cfg(all(
    target_os = "macos",
    guest_arch = "aarch64",
    guest_is_native,
    feature = "virt_hvf"
))]

use hypervisor_resources::HvfHandle;
use hypervisor_resources::HypervisorKind;
use vm_resource::Resource;

/// HVF probe for auto-detection.
pub struct HvfProbe;

impl hypervisor_resources::HypervisorProbe for HvfProbe {
    fn name(&self) -> &str {
        "hvf"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        Ok(Some(Resource::new(HvfHandle)))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        if let Some(&(key, _)) = params.first() {
            anyhow::bail!("unknown hvf parameter: {key}");
        }
        Ok(Resource::new(HvfHandle))
    }
}
