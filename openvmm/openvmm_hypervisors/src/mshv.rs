// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]

use crate::parse_bool_param;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

fn parse_mshv_params(params: &[(&str, &str)]) -> anyhow::Result<bool> {
    let mut snp_disable_cpuid_offload = false;
    for &(key, value) in params {
        match key {
            "snp_disable_cpuid_offload" => {
                snp_disable_cpuid_offload = parse_bool_param(key, value)?;
            }
            _ => anyhow::bail!("unknown mshv parameter: {key}"),
        }
    }
    Ok(snp_disable_cpuid_offload)
}

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
        Ok(Some(
            MshvHandle {
                mshv: mshv.into(),
                snp_disable_cpuid_offload: false,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let snp_disable_cpuid_offload = parse_mshv_params(params)?;
        anyhow::ensure!(virt_mshv::is_available()?, "MSHV is not available");
        Ok(Resource::new(MshvHandle {
            mshv: fs_err::File::open("/dev/mshv")?.into(),
            snp_disable_cpuid_offload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn parses_snp_cpuid_offload_parameter() {
        assert!(!parse_mshv_params(&[]).unwrap());
        assert!(parse_mshv_params(&[("snp_disable_cpuid_offload", "true")]).unwrap());
        assert!(parse_mshv_params(&[("unknown", "true")]).is_err());
    }
}
