// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

impl hypervisor_resources::HypervisorProbe for KvmProbe {
    fn name(&self) -> &str {
        "kvm"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let kvm = match fs_err::File::options()
            .read(true)
            .write(true)
            .open("/dev/kvm")
        {
            Ok(kvm) => kvm,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(KvmHandle { kvm: kvm.into() }.into_resource()))
    }
}

/// KVM resource resolver.
pub struct KvmResolver;

impl vm_resource::ResolveResource<HypervisorKind, KvmHandle> for KvmResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = virt_kvm::KvmError;

    fn resolve(&self, resource: KvmHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        let kvm = resource.kvm;
        Ok(ResolvedHypervisorBackend::new(virt_kvm::Kvm::from_kvm(
            kvm,
        )?))
    }
}

vm_resource::declare_static_resolver!(KvmResolver, (HypervisorKind, KvmHandle),);
