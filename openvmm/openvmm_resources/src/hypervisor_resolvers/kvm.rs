// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM resource resolver.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;

/// KVM resource resolver.
pub struct KvmResolver;

impl vm_resource::ResolveResource<HypervisorKind, KvmHandle> for KvmResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = virt_kvm::KvmError;

    fn resolve(&self, resource: KvmHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        let kvm = resource.kvm;
        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_mut))]
        let mut backend = virt_kvm::Kvm::from_kvm(kvm)?;
        #[cfg(guest_arch = "x86_64")]
        {
            backend.nested_virt = resource.nested_virt;
        }
        Ok(ResolvedHypervisorBackend::new(backend))
    }
}

vm_resource::declare_static_resolver!(KvmResolver, (HypervisorKind, KvmHandle),);
