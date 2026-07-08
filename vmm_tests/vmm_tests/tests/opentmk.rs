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

        // Capture the result so teardown always runs, even on failure: OpenTMK
        // never powers the guest off, so we must not leak a running VM.
        let result = vm
            .wait_for_opentmk(OPENTMK_TIMEOUT)
            .await
            .and_then(|run| petri::opentmk::evaluate(&run));

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

    /// Defines a Hyper-V + OpenHCL OpenTMK test that runs the guest-internal
    /// scenario `$test` under the given `$isolation`.
    macro_rules! opentmk_test {
        ($name:ident, $test:literal, $isolation:expr) => {
            petri::test!($name, |resolver| {
                resolve_opentmk_openhcl::<HyperVPetriBackend>(resolver, $test, $isolation)
            });

            fn $name(
                params: petri::PetriTestParams<'_>,
                artifacts: OpentmkArtifacts<HyperVPetriBackend>,
            ) -> anyhow::Result<()> {
                run_opentmk_uefi(params, artifacts)
            }
        };
    }

    // Baseline VP-count scenario on a non-isolated VM and on SNP/TDX CVMs.
    opentmk_test!(opentmk_hyperv_openhcl_uefi_x64, "hv_processor", None);
    opentmk_test!(
        opentmk_hyperv_openhcl_uefi_x64_snp,
        "hv_processor",
        Some(IsolationType::Snp)
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_uefi_x64_tdx,
        "hv_processor",
        Some(IsolationType::Tdx)
    );

    // Interrupt scenarios that run on a non-isolated OpenHCL VTL guest.
    opentmk_test!(
        opentmk_hyperv_openhcl_memory_protect_read,
        "hv_memory_protect_read",
        None
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_memory_protect_write,
        "hv_memory_protect_write",
        None
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_register_intercept,
        "hv_register_intercept",
        None
    );

    // Interrupt scenarios that require a confidential VM (TPM), on SNP and TDX.
    opentmk_test!(
        opentmk_hyperv_openhcl_tpm_read_cvm_snp,
        "hv_tpm_read_cvm",
        Some(IsolationType::Snp)
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_tpm_write_cvm_snp,
        "hv_tpm_write_cvm",
        Some(IsolationType::Snp)
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_tpm_read_cvm_tdx,
        "hv_tpm_read_cvm",
        Some(IsolationType::Tdx)
    );
    opentmk_test!(
        opentmk_hyperv_openhcl_tpm_write_cvm_tdx,
        "hv_tpm_write_cvm",
        Some(IsolationType::Tdx)
    );
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
