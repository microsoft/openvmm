// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

impl hypervisor_resources::HypervisorProbe for KvmProbe {
    fn name(&self) -> &str {
        "kvm"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        Ok(virt_kvm::is_available()?.then(|| Resource::new(KvmHandle)))
    }
}

/// KVM resource resolver.
pub struct KvmResolver;

impl vm_resource::ResolveResource<HypervisorKind, KvmHandle> for KvmResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = std::convert::Infallible;

    fn resolve(&self, _resource: KvmHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedHypervisorBackend::new(virt_kvm::Kvm))
    }
}

vm_resource::declare_static_resolver!(KvmResolver, (HypervisorKind, KvmHandle),);
