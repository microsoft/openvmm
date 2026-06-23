// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV resource resolver.

#![cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;

/// MSHV resource resolver.
pub struct MshvResolver;

impl vm_resource::ResolveResource<HypervisorKind, MshvHandle> for MshvResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, resource: MshvHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_mshv::LinuxMshv::from(
            resource.mshv,
        )))
    }
}

vm_resource::declare_static_resolver!(MshvResolver, (HypervisorKind, MshvHandle),);
