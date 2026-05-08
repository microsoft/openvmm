// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// MSHV probe for auto-detection.
pub struct MshvProbe;

impl hypervisor_resources::HypervisorProbe for MshvProbe {
    fn name(&self) -> &str {
        "mshv"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let mshv = match fs_err::File::open("/dev/mshv") {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(MshvHandle { mshv: mshv.into() }.into_resource()))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        if let Some(&(key, _)) = params.first() {
            anyhow::bail!("unknown mshv parameter: {key}");
        }
        anyhow::ensure!(virt_mshv::is_available()?, "MSHV is not available");
        Ok(Resource::new(MshvHandle {
            mshv: fs_err::File::open("/dev/mshv")?.into(),
        }))
    }
}

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
