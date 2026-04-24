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
        Ok(virt_whp::is_available()?.then(|| Resource::new(WhpHandle::default())))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let mut handle = WhpHandle::default();
        for &(key, val) in params {
            match key {
                "user_mode_apic" => {
                    if cfg!(guest_arch = "x86_64") {
                        handle.user_mode_apic = parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("whp parameter {key} is only supported for x86_64 guests");
                    }
                }
                "no_enlightenments" => {
                    if cfg!(guest_arch = "x86_64") {
                        handle.offload_enlightenments = !parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("whp parameter {key} is only supported for x86_64 guests");
                    }
                }
                _ => anyhow::bail!("unknown whp parameter: {key}"),
            }
        }
        anyhow::ensure!(virt_whp::is_available()?, "WHP is not available");
        Ok(Resource::new(handle))
    }
}

fn parse_bool_param(key: &str, val: &str) -> anyhow::Result<bool> {
    match val {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => anyhow::bail!("invalid boolean value for {key}: {val}"),
    }
}

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
