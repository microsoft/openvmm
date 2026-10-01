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
        let backend = virt_kvm::Kvm::from_kvm(kvm)?;
        #[cfg(guest_arch = "x86_64")]
        let backend = {
            let mut backend = backend;
            backend.force_tsc_fallback(resource.force_tsc_fallback);
            backend
        };
        #[cfg(guest_arch = "aarch64")]
        if resource.force_tsc_fallback {
            return Err(virt_kvm::KvmError::NotSupported);
        }
        Ok(ResolvedHypervisorBackend::new(backend))
    }
}

vm_resource::declare_static_resolver!(KvmResolver, (HypervisorKind, KvmHandle),);
