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
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
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
}

/// HVF resource resolver.
pub struct HvfResolver;

impl vm_resource::ResolveResource<HypervisorKind, HvfHandle> for HvfResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, _resource: HvfHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_hvf::HvfHypervisor))
    }
}

vm_resource::declare_static_resolver!(HvfResolver, (HypervisorKind, HvfHandle),);
