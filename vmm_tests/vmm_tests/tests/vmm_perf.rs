// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMM.Perf profiles executed through the Petri/nextest test harness.

#![forbid(unsafe_code)]

// Compile the implementation on Windows so it stays validated while runtime
// artifact registration remains Linux-only.
#[cfg(all(
    target_arch = "x86_64", // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
    any(target_os = "linux", target_os = "windows")
))]
#[cfg_attr(target_os = "windows", expect(dead_code))]
mod vmm_perf {
    mod command;
    mod config;
    mod diagnostics;
    mod host;
    mod runner;
    mod runtime;
    mod virtual_client;

    #[cfg(target_os = "linux")]
    use config::VmmPerfProfile;
    #[cfg(target_os = "linux")]
    use runner::VmmPerfArtifacts;
    #[cfg(target_os = "linux")]
    use runner::VmmPerfRunner;

    #[cfg(target_os = "linux")]
    fn resolve_vmm_perf(resolver: &petri::ArtifactResolver<'_>) -> Option<VmmPerfArtifacts> {
        use petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY;
        use petri_artifacts_vmm_test::artifacts;

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

    #[cfg(target_os = "linux")]
    fn run_fio(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::Fio)
    }

    #[cfg(target_os = "linux")]
    fn run_iperf3(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::Iperf3)
    }

    #[cfg(target_os = "linux")]
    fn run_boot_time(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        VmmPerfRunner::new(params, artifacts)?.run(VmmPerfProfile::BootTime)
    }

    #[cfg(target_os = "linux")]
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
