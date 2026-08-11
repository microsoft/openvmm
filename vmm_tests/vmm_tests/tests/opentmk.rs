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
    /// Whether to attach a vTPM to the VM. Required by the `hv_tpm_*` scenarios,
    /// which expect UEFI to expose the TCG protocol and a TPM command buffer.
    tpm: bool,
}

/// Build the embedded config selecting the `hyperv` backend and the given
/// `test`. Must match `opentmk_protocol::TestConfig`.
fn opentmk_config_json(test: &str) -> Vec<u8> {
    // Build via serde_json so the test name is escaped and the payload stays
    // well-formed if fields are added.
    serde_json::to_vec(&serde_json::json!({ "backend": "hyperv", "test": test }))
        .expect("opentmk config serialization cannot fail")
}

/// Build the host requirements for an OpenTMK test with the given `isolation`.
///
/// Non-isolated tests can run on any host; isolated variants declare an
/// [`petri::requirements::TestRequirement::Isolation`] requirement so they are
/// only selected on hosts that support the corresponding isolation type (e.g.
/// SNP/TDX CVM runners) and are automatically excluded elsewhere.
fn opentmk_requirements(
    isolation: Option<petri::IsolationType>,
) -> petri::requirements::TestCaseRequirements {
    use petri::requirements::ExecutionEnvironment;
    use petri::requirements::IsolationType as ReqIsolationType;
    use petri::requirements::TestRequirement;

    let requirement = match isolation {
        None => TestRequirement::Any,
        // Hyper-V VBS does not work nested, so also require a baremetal host
        // (matching the `#[vmm_test]` macro behavior for hyper-v + vbs).
        Some(petri::IsolationType::Vbs) => TestRequirement::Isolation(ReqIsolationType::Vbs).and(
            TestRequirement::ExecutionEnvironment(ExecutionEnvironment::Baremetal),
        ),
        Some(petri::IsolationType::Snp) => TestRequirement::Isolation(ReqIsolationType::Snp),
        Some(petri::IsolationType::Tdx) => TestRequirement::Isolation(ReqIsolationType::Tdx),
    };

    petri::requirements::TestCaseRequirements::new(requirement)
}

/// Resolve the artifacts to boot OpenTMK (selecting `test`) on Hyper-V with
/// OpenHCL, optionally as a confidential VM. Returns `None` if the host can't
/// run it (non-x86_64).
///
/// `tpm` attaches a vTPM to the VM; the `hv_tpm_*` scenarios require it.
fn resolve_opentmk_openhcl<T: PetriVmmBackend>(
    resolver: &petri::ArtifactResolver<'_>,
    test: &str,
    isolation: Option<petri::IsolationType>,
    tpm: bool,
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
    Some(OpentmkArtifacts { vm, tpm })
}

/// Boot the OpenTMK guest, wait for its COM1 results, and assert every check passed.
fn run_opentmk_uefi<T: PetriVmmBackend>(
    params: petri::PetriTestParams<'_>,
    artifacts: OpentmkArtifacts<T>,
) -> anyhow::Result<()> {
    use petri::PetriVmBuilder;
    use petri::ProcessorTopology;
    pal_async::DefaultPool::run_with(async |driver| {
        let mut builder = PetriVmBuilder::new(params, artifacts.vm, &driver)?
            .with_expect_no_boot_event()
            .with_processor_topology(ProcessorTopology {
                vp_count: OPENTMK_VP_COUNT,
                ..Default::default()
            });
        if artifacts.tpm {
            builder = builder.with_tpm(true);
        }
        let mut vm = builder.run_without_agent().await?;

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
    use crate::resolve_opentmk_openhcl;
    use crate::run_opentmk_uefi;
    use petri::IsolationType;
    use petri::hyperv::HyperVPetriBackend;

    /// Maps an isolation token (`none`/`snp`/`tdx`) to its
    /// `Option<petri::IsolationType>` value.
    macro_rules! opentmk_isolation {
        (none) => {
            None
        };
        (vbs) => {
            Some(IsolationType::Vbs)
        };
        (snp) => {
            Some(IsolationType::Snp)
        };
        (tdx) => {
            Some(IsolationType::Tdx)
        };
    }

    /// Defines one or more Hyper-V + OpenHCL OpenTMK tests for the guest-internal
    /// scenario `$test`, one per isolation token in the `[..]` list.
    ///
    /// Each generated test is named `$base` plus an isolation suffix (`_snp` /
    /// `_tdx`, no suffix for `none`). Isolated variants declare an isolation host
    /// requirement so they only run on isolation-capable CI runners and are
    /// skipped elsewhere. `$tpm` attaches a vTPM to the VM (required by the
    /// `hv_tpm_*` scenarios).
    macro_rules! opentmk_test {
        ($base:literal, $test:literal, $tpm:expr, [$($iso:tt),+ $(,)?]) => {
            ::petri::multitest!(vec![
                $( opentmk_test!(@case $base, $test, $tpm, $iso) ),+
            ]);
        };
        (@case $base:literal, $test:literal, $tpm:expr, none) => {
            opentmk_test!(@build concat!($base), $test, $tpm, none)
        };
        (@case $base:literal, $test:literal, $tpm:expr, vbs) => {
            opentmk_test!(@build concat!($base, "_vbs"), $test, $tpm, vbs)
        };
        (@case $base:literal, $test:literal, $tpm:expr, snp) => {
            opentmk_test!(@build concat!($base, "_snp"), $test, $tpm, snp)
        };
        (@case $base:literal, $test:literal, $tpm:expr, tdx) => {
            opentmk_test!(@build concat!($base, "_tdx"), $test, $tpm, tdx)
        };
        (@build $name:expr, $test:literal, $tpm:expr, $iso:tt) => {
            ::petri::SimpleTest::new(
                $name,
                |resolver| {
                    resolve_opentmk_openhcl::<HyperVPetriBackend>(
                        resolver,
                        $test,
                        opentmk_isolation!($iso),
                        $tpm,
                    )
                },
                run_opentmk_uefi::<HyperVPetriBackend>,
            )
            .requirements(crate::opentmk_requirements(opentmk_isolation!($iso)))
            .into()
        };
    }

    opentmk_test!(
        "opentmk_hyperv_openhcl_uefi_x64",
        "hv_processor",
        false,
        [none, vbs, snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_memory_protect_read",
        "hv_memory_protect_read",
        false,
        [none, vbs, snp, tdx]
    );
    opentmk_test!(
        "opentmk_hyperv_openhcl_memory_protect_write",
        "hv_memory_protect_write",
        false,
        [none, vbs, snp, tdx]
    );
    opentmk_test!(
        "opentmk_hyperv_openhcl_register_intercept",
        "hv_register_intercept",
        false,
        [none, vbs, snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_tpm_read_cvm",
        "hv_tpm_read_cvm",
        true,
        [snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_tpm_write_cvm",
        "hv_tpm_write_cvm",
        true,
        [snp, tdx]
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
