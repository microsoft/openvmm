// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use anyhow::Context as _;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

impl hypervisor_resources::HypervisorProbe for KvmProbe {
    fn name(&self) -> &str {
        "kvm"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let kvm = match open_kvm() {
            Ok(kvm) => kvm,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(
            KvmHandle {
                kvm: kvm.into(),
                nested_virt: false,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let mut nested_virt = false;
        for &(key, val) in params {
            match key {
                "nested_virt" => {
                    if cfg!(guest_arch = "x86_64") {
                        nested_virt = parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                }
                _ => anyhow::bail!("unknown kvm parameter: {key}"),
            }
        }
        let kvm = open_kvm().context("KVM is not available")?;
        Ok(KvmHandle {
            kvm: kvm.into(),
            nested_virt,
        }
        .into_resource())
    }
}

fn open_kvm() -> std::io::Result<fs_err::File> {
    fs_err::File::options()
        .read(true)
        .write(true)
        .open("/dev/kvm")
}

use crate::parse_bool_param;
