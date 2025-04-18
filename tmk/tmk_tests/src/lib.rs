// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test code for running TMK tests in different environments.

use anyhow::Context as _;
use pal_async::DefaultPool;
use pal_async::task::Spawn as _;
use petri::ResolvedArtifact;

petri::test!(host_tmks, |resolver| {
    let tmk_vmm = resolver
        .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_NATIVE)
        .erase();
    let tmk = if cfg!(guest_arch = "x86_64") {
        resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_X64)
            .erase()
    } else if cfg!(guest_arch = "aarch64") {
        resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64)
            .erase()
    } else {
        panic!("Unsupported guest architecture");
    };
    (tmk_vmm, tmk)
});

fn host_tmks(
    params: petri::PetriTestParams<'_>,
    (tmk_vmm, tmk): (ResolvedArtifact, ResolvedArtifact),
) -> anyhow::Result<()> {
    let (_, driver) = DefaultPool::spawn_on_thread("pool");
    let (stdout, stdout_write) = pal_async::pipe::PolledPipe::pair(&driver)?;
    driver
        .spawn(
            "log",
            petri::log_stream(params.logger.log_file("tmk_vmm")?, stdout),
        )
        .detach();

    let output = std::process::Command::new(tmk_vmm)
        .arg("--tmk")
        .arg(tmk)
        .stdout(stdout_write.into_inner())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to launch tmk_vmm")?;

    if !output.status.success() {
        anyhow::bail!(
            "tmk_vmm exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

petri::test!(openvmm_openhcl_tmks, resolve_openvmm_openhcl_tmks);

struct OpenvmmOpenhclArtifacts {
    vm: petri::openvmm::PetriVmArtifactsOpenVmm,
    tmk_vmm: ResolvedArtifact,
    tmk: ResolvedArtifact,
}

fn resolve_openvmm_openhcl_tmks(resolver: &petri::ArtifactResolver<'_>) -> OpenvmmOpenhclArtifacts {
    let igvm_path;
    let tmk_vmm;
    let tmk;

    if cfg!(guest_arch = "x86_64") {
        igvm_path = resolver
            .require(petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64)
            .erase();
        tmk_vmm = resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64_MUSL)
            .erase();
        tmk = resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_X64)
            .erase();
    } else if cfg!(guest_arch = "aarch64") {
        igvm_path = resolver
            .require(petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64)
            .erase();
        tmk_vmm = resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL)
            .erase();
        tmk = resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64)
            .erase();
    } else {
        panic!("Unsupported guest architecture");
    };

    let vm = petri::openvmm::PetriVmArtifactsOpenVmm::new(
        resolver,
        petri::Firmware::OpenhclUefi {
            guest: petri::UefiGuest::None,
            isolation: None,
            vtl2_nvme_boot: false,
            igvm_path,
        },
        petri_artifacts_common::tags::MachineArch::X86_64,
    );
    OpenvmmOpenhclArtifacts { vm, tmk_vmm, tmk }
}

fn openvmm_openhcl_tmks(
    params: petri::PetriTestParams<'_>,
    artifacts: OpenvmmOpenhclArtifacts,
) -> anyhow::Result<()> {
    DefaultPool::run_with(async |driver| {
        let mut vm = petri::openvmm::PetriVmConfigOpenVmm::new(&params, artifacts.vm, &driver)?
            .with_openhcl_command_line("OPENHCL_WAIT_FOR_START=1")
            .with_openhcl_agent_file("tmk_vmm", artifacts.tmk_vmm)
            .with_openhcl_agent_file("simple_tmk", artifacts.tmk)
            .with_processors(1)
            .with_allow_early_vtl0_access(true) // TODO: remove once the TMK VMM initializes memory properly.
            .run_without_agent()
            .await?;

        let agent = vm.wait_for_vtl2_agent().await?;
        let mut child = agent
            .command("/cidata/tmk_vmm")
            .arg("--tmk")
            .arg("/cidata/simple_tmk")
            .arg("--hv")
            .arg("mshv-vtl")
            .stdout(petri::pipette::process::Stdio::piped())
            .stderr(petri::pipette::process::Stdio::piped())
            .spawn()
            .await?;

        driver
            .spawn(
                "log",
                petri::log_stream(
                    params.logger.log_file("tmk_vmm")?,
                    child.stdout.take().unwrap(),
                ),
            )
            .detach();

        let output = child.wait_with_output().await?;
        if !output.status.success() {
            anyhow::bail!(
                "tmk_vmm exited with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    })
}
