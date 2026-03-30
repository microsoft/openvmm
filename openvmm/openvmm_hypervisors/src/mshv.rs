// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV hypervisor backend.

#![cfg(all(
    target_os = "linux",
    feature = "virt_mshv",
    guest_is_native,
    guest_arch = "x86_64"
))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use vm_resource::Resource;

/// MSHV probe for auto-detection.
pub struct MshvProbe;

impl hypervisor_resources::HypervisorProbe for MshvProbe {
    fn name(&self) -> &str {
        "mshv"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        Ok(virt_mshv::is_available()?.then(|| Resource::new(MshvHandle)))
    }
}

/// MSHV resource resolver.
pub struct MshvResolver;

impl vm_resource::ResolveResource<HypervisorKind, MshvHandle> for MshvResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, _resource: MshvHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_mshv::LinuxMshv))
    }
}

vm_resource::declare_static_resolver!(MshvResolver, (HypervisorKind, MshvHandle),);
