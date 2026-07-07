// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test entrypoint for running OpenTMK guest tests.
//!
//! Each test boots a Hyper-V VM with OpenHCL as the paravisor and a specific
//! OpenTMK test selection (patched in by [`petri::UefiGuest::opentmk`]), waits
//! for the guest to stream its results over COM1, and asserts every check
//! passed. Variants cover a non-isolated VM and SNP/TDX confidential VMs.

#![forbid(unsafe_code)]
// Hyper-V is only available on Windows, so the whole suite is Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

use petri::PetriVmArtifacts;
use petri::PetriVmmBackend;
use petri_artifacts_common::tags::MachineArch;
use std::time::Duration;

/// OpenTMK's `hyperv`-backend tests require exactly four VPs (assert `vp_count == 4`).
const OPENTMK_VP_COUNT: u32 = 4;

/// How long to wait for the guest to stream its results before giving up.
const OPENTMK_TIMEOUT: Duration = Duration::from_secs(240);

/// Artifacts required to boot an OpenTMK UEFI guest on backend `T`.
struct OpentmkArtifacts<T: PetriVmmBackend> {
    vm: PetriVmArtifacts<T>,
}

/// Build the embedded config selecting the `hyperv` backend and the given
/// `test`. Must match `opentmk_protocol::TestConfig`.
fn opentmk_config_json(test: &str) -> Vec<u8> {
    format!(r#"{{"backend":"hyperv","test":"{test}"}}"#).into_bytes()
}

/// Resolve the artifacts to boot OpenTMK (selecting `test`) on Hyper-V with
/// OpenHCL, optionally as a confidential VM. Returns `None` if the host can't
/// run it (non-x86_64).
fn resolve_opentmk_openhcl<T: PetriVmmBackend>(
    resolver: &petri::ArtifactResolver<'_>,
    test: &str,
    isolation: Option<petri::IsolationType>,
) -> Option<OpentmkArtifacts<T>> {
    let arch = MachineArch::host();
    if arch != MachineArch::X86_64 {
        return None;
    }
    let guest = petri::UefiGuest::opentmk(resolver, arch, &opentmk_config_json(test));
    let vm = PetriVmArtifacts::new(
        resolver,
        petri::Firmware::openhcl_uefi(resolver, arch, guest, isolation),
        arch,
        false,
    )?;
    Some(OpentmkArtifacts { vm })
}

/// Boot the OpenTMK guest, wait for its COM1 results, and assert every check passed.
fn run_opentmk_uefi<T: PetriVmmBackend>(
    params: petri::PetriTestParams<'_>,
    artifacts: OpentmkArtifacts<T>,
) -> anyhow::Result<()> {
    use petri::PetriVmBuilder;
    use petri::ProcessorTopology;
    pal_async::DefaultPool::run_with(async |driver| {
        let mut vm = PetriVmBuilder::new(params, artifacts.vm, &driver)?
            .with_expect_no_boot_event()
            .with_processor_topology(ProcessorTopology {
                vp_count: OPENTMK_VP_COUNT,
                ..Default::default()
            })
            .run_without_agent()
            .await?;

        let run = vm.wait_for_opentmk(OPENTMK_TIMEOUT).await?;
        let result = petri::opentmk::evaluate(&run);

        // OpenTMK never powers the guest off, so always tear it down explicitly,
        // even on failure, rather than leaking a running VM.
        vm.teardown().await?;

        result
    })
}

#[cfg(windows)]
mod hyperv {
    use crate::OpentmkArtifacts;
    use crate::resolve_opentmk_openhcl;
    use crate::run_opentmk_uefi;
    use petri::IsolationType;
    use petri::hyperv::HyperVPetriBackend;

    petri::test!(opentmk_hyperv_openhcl_uefi_x64, |resolver| {
        resolve_opentmk_openhcl::<HyperVPetriBackend>(resolver, "hv_processor", None)
    });

    fn opentmk_hyperv_openhcl_uefi_x64(
        params: petri::PetriTestParams<'_>,
        artifacts: OpentmkArtifacts<HyperVPetriBackend>,
    ) -> anyhow::Result<()> {
        run_opentmk_uefi(params, artifacts)
    }

    petri::test!(opentmk_hyperv_openhcl_uefi_x64_snp, |resolver| {
        resolve_opentmk_openhcl::<HyperVPetriBackend>(
            resolver,
            "hv_processor",
            Some(IsolationType::Snp),
        )
    });

    fn opentmk_hyperv_openhcl_uefi_x64_snp(
        params: petri::PetriTestParams<'_>,
        artifacts: OpentmkArtifacts<HyperVPetriBackend>,
    ) -> anyhow::Result<()> {
        run_opentmk_uefi(params, artifacts)
    }

    petri::test!(opentmk_hyperv_openhcl_uefi_x64_tdx, |resolver| {
        resolve_opentmk_openhcl::<HyperVPetriBackend>(
            resolver,
            "hv_processor",
            Some(IsolationType::Tdx),
        )
    });

    fn opentmk_hyperv_openhcl_uefi_x64_tdx(
        params: petri::PetriTestParams<'_>,
        artifacts: OpentmkArtifacts<HyperVPetriBackend>,
    ) -> anyhow::Result<()> {
        run_opentmk_uefi(params, artifacts)
    }
}

#[cfg(windows)]
fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}

#[cfg(not(windows))]
fn main() {}
