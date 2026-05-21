// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HVF (macOS Hypervisor.framework) resource resolver.

#![cfg(all(
    target_os = "macos",
    guest_arch = "aarch64",
    guest_is_native,
    feature = "virt_hvf"
))]

use hypervisor_resources::HvfHandle;
use hypervisor_resources::HypervisorKind;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;

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
