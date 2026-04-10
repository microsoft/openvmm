// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP hypervisor backend.

#![cfg(all(target_os = "windows", feature = "virt_whp", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::WhpHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use vm_resource::Resource;

/// WHP probe for auto-detection.
pub struct WhpProbe;

impl hypervisor_resources::HypervisorProbe for WhpProbe {
    fn name(&self) -> &str {
        "whp"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        Ok(virt_whp::is_available()?.then(|| Resource::new(WhpHandle)))
    }
}

/// WHP resource resolver.
pub struct WhpResolver;

impl vm_resource::ResolveResource<HypervisorKind, WhpHandle> for WhpResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, _resource: WhpHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_whp::Whp))
    }
}

vm_resource::declare_static_resolver!(WhpResolver, (HypervisorKind, WhpHandle),);
