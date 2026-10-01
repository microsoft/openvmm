// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use crate::parse_bool_param;
use anyhow::Context as _;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

fn parse_kvm_params(params: &[(&str, &str)]) -> anyhow::Result<bool> {
    let mut force_tsc_fallback = false;
    for &(key, value) in params {
        match key {
            "force_tsc_fallback" => {
                anyhow::ensure!(
                    cfg!(guest_arch = "x86_64"),
                    "kvm parameter {key} is only supported for x86_64 guests"
                );
                force_tsc_fallback = parse_bool_param(key, value)?;
            }
            _ => anyhow::bail!("unknown kvm parameter: {key}"),
        }
    }
    Ok(force_tsc_fallback)
}

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
                force_tsc_fallback: false,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let force_tsc_fallback = parse_kvm_params(params)?;
        let kvm = open_kvm().context("KVM is not available")?;
        Ok(KvmHandle {
            kvm: kvm.into(),
            force_tsc_fallback,
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn parses_tsc_fallback_parameter() {
        assert!(!parse_kvm_params(&[]).unwrap());
        assert!(parse_kvm_params(&[("unknown", "true")]).is_err());
        assert!(parse_kvm_params(&[("force_tsc_fallback", "invalid")]).is_err());
        if cfg!(guest_arch = "x86_64") {
            for value in ["true", "1", "yes"] {
                assert!(parse_kvm_params(&[("force_tsc_fallback", value)]).unwrap());
            }
            for value in ["false", "0", "no"] {
                assert!(!parse_kvm_params(&[("force_tsc_fallback", value)]).unwrap());
            }
        } else {
            assert!(parse_kvm_params(&[("force_tsc_fallback", "true")]).is_err());
            assert!(parse_kvm_params(&[("force_tsc_fallback", "false")]).is_err());
        }
    }
}
