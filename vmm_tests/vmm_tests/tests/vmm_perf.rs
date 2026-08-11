// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMM.Perf profiles executed through the Petri/nextest test harness.

#![forbid(unsafe_code)]

// xtask-fmt allow-target-arch oneoff-petri-native-test-deps
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod vmm_perf {
    mod command;
    mod config;
    mod diagnostics;
    mod host;
    mod runner;
    mod runtime;
    mod virtual_client;

    use config::VmmPerfProfile;
    use petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY;
    use petri_artifacts_vmm_test::artifacts;
    use runner::VmmPerfArtifacts;
    use runner::VmmPerfRunner;

    fn resolve_vmm_perf(resolver: &petri::ArtifactResolver<'_>) -> Option<VmmPerfArtifacts> {
        Some(VmmPerfArtifacts {
            openvmm: resolver.require(artifacts::OPENVMM_NATIVE).erase(),
            firmware: resolver
                .require(artifacts::loadable::UEFI_FIRMWARE_X64)
                .erase(),
            runtime_archive: resolver
                .require(artifacts::vmm_perf::RUNTIME_NATIVE)
                .erase(),
            log_dir: resolver.require(TEST_LOG_DIRECTORY).erase(),
        })
    }

    fn run_fio(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::Fio)
    }

    fn run_iperf3(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::Iperf3)
    }

    fn run_boot_time(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::BootTime)
    }

    petri::multitest!(vec![
        petri::SimpleTest::new("fio", resolve_vmm_perf, run_fio).into(),
        petri::SimpleTest::new("iperf3", resolve_vmm_perf, run_iperf3).into(),
        petri::SimpleTest::new("boot_time", resolve_vmm_perf, run_boot_time).into(),
    ]);
}

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
