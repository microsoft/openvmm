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
                hv_spec: None,
                cpu_model: None,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        // Capture the raw `hv=`/`cpu=` kvm parameters. The Hyper-V enlightenment
        // set is resolved later, at partition creation (virt_kvm `new_partition`),
        // where the generic `nested_virt` request is known: the `windows` preset
        // is nested-aware, and upstream #3869 moved nested_virt off the kvm handle
        // onto the shared partition config, sourced from the generic `--nested-virt`
        // flag rather than a `kvm:nested_virt` parameter. So this parser no longer
        // sees nested_virt and only carries the raw inputs forward.
        let mut hv_spec = None;
        let mut cpu_model = None;
        for &(key, val) in params {
            match key {
                "hv" => {
                    if cfg!(guest_arch = "x86_64") {
                        hv_spec = Some(val.to_owned());
                    } else {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                }
                "cpu" => {
                    if !cfg!(guest_arch = "x86_64") {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                    if val.is_empty() {
                        anyhow::bail!("kvm cpu parameter requires a model name");
                    }
                    cpu_model = Some(val.to_owned());
                }
                _ => anyhow::bail!("unknown kvm parameter: {key}"),
            }
        }

        let kvm = open_kvm().context("KVM is not available")?;
        Ok(KvmHandle {
            kvm: kvm.into(),
            hv_spec,
            cpu_model,
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
