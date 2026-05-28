// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP resource resolver.

#![cfg(all(target_os = "windows", feature = "virt_whp", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::WhpHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;

/// WHP resource resolver.
pub struct WhpResolver;

impl vm_resource::ResolveResource<HypervisorKind, WhpHandle> for WhpResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, resource: WhpHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_whp::Whp {
            user_mode_apic: resource.user_mode_apic,
            offload_enlightenments: resource.offload_enlightenments,
        }))
    }
}

vm_resource::declare_static_resolver!(WhpResolver, (HypervisorKind, WhpHandle),);
