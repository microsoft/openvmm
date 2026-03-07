// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test code for running TMK tests in different environments.

#![forbid(unsafe_code)]

use anyhow::Context as _;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::task::Spawn as _;
use petri::PetriVm;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_common::tags::MachineArch;

petri::test!(host_tmks, |resolver| resolve_host_tmks(resolver, false));

fn host_tmks(
    params: petri::PetriTestParams<'_>,
    artifacts: (ResolvedArtifact, ResolvedArtifact),
) -> anyhow::Result<()> {
    host_tmks_core(params, false, artifacts)
}

petri::test!(host_tmks_emulated_apic, |resolver| resolve_host_tmks(
    resolver, true
));

fn host_tmks_emulated_apic(
    params: petri::PetriTestParams<'_>,
    artifacts: (ResolvedArtifact, ResolvedArtifact),
) -> anyhow::Result<()> {
    host_tmks_core(params, true, artifacts)
}

fn resolve_simple_tmk(
    resolver: &petri::ArtifactResolver<'_>,
    arch: MachineArch,
) -> ResolvedArtifact {
    match arch {
        MachineArch::X86_64 => resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_X64)
            .erase(),
        MachineArch::Aarch64 => resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64)
            .erase(),
    }
}

fn resolve_host_tmks(
    resolver: &petri::ArtifactResolver<'_>,
    emulated_apic: bool,
) -> Option<(ResolvedArtifact, ResolvedArtifact)> {
    // Only useful on virt_whp x64 for now.
    if emulated_apic && (MachineArch::host() != MachineArch::X86_64 || !cfg!(windows)) {
        return None;
    }
    let tmk_vmm = resolver
        .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_NATIVE)
        .erase();
    Some((tmk_vmm, resolve_simple_tmk(resolver, MachineArch::host())))
}

fn host_tmks_core(
    params: petri::PetriTestParams<'_>,
    disable_offloads: bool,
    (tmk_vmm, tmk): (ResolvedArtifact, ResolvedArtifact),
) -> anyhow::Result<()> {
    let (_, driver) = DefaultPool::spawn_on_thread("pool");
    let (stdout, stdout_write) = pal_async::pipe::PolledPipe::pair(&driver)?;
    driver
        .spawn(
            "log",
            petri::log_task(params.logger.log_file("tmk_vmm")?, stdout, "tmk_vmm stdout"),
        )
        .detach();

    let mut cmd = std::process::Command::new(tmk_vmm);
    if disable_offloads {
        cmd.arg("--disable-offloads");
    }

    let output = cmd
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

fn resolve_paravisor_tmk_artifacts(
    resolver: &petri::ArtifactResolver<'_>,
    arch: MachineArch,
) -> (ResolvedArtifact, ResolvedArtifact, ResolvedArtifact) {
    let igvm_path;
    let tmk_vmm;

    match arch {
        MachineArch::X86_64 => {
            igvm_path = resolver
                .require(petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64)
                .erase();
            tmk_vmm = resolver
                .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64_MUSL)
                .erase();
        }
        MachineArch::Aarch64 => {
            igvm_path = resolver
                .require(petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64)
                .erase();
            tmk_vmm = resolver
                .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL)
                .erase();
        }
    }

    (igvm_path, tmk_vmm, resolve_simple_tmk(resolver, arch))
}

/// The OpenHCL command line to use for the TMK VMM. This is used to:
/// 1. Suspend launching the OpenHCL VMM so that the TMK VMM can be launched instead.
/// 2. Report to the host that VTL0 has been started so that Hyper-V VM start does not hang.
/// 3. Wait for modules to be loaded before lauching the diagnostics service (used to launch the TMK VMM),
///    so that the virtual disk that the TMK VMM is launched from finishes initializing.
const OPENHCL_COMMAND_LINE: &str =
    "OPENHCL_WAIT_FOR_START=1 OPENHCL_SIGNAL_VTL0_STARTED=1 OPENHCL_WAIT_FOR_MODULES=1";

async fn openhcl_tmks_inner<T: PetriVmmBackend>(
    driver: &DefaultDriver,
    logger: petri::PetriLogSource,
    vm: &mut PetriVm<T>,
) -> anyhow::Result<()> {
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
            petri::log_task(
                logger.log_file("tmk_vmm")?,
                child.stdout.take().unwrap(),
                "tmk_vmm stdout",
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
}

petri::test!(
    openvmm_openhcl_tmks,
    resolve_openhcl_tmks::<OpenVmmPetriBackend>
);

struct OpenhclTmkArtifacts<T: PetriVmmBackend> {
    vm: petri::PetriVmArtifacts<T>,
    tmk_vmm: ResolvedArtifact,
    tmk: ResolvedArtifact,
}

fn resolve_openhcl_tmks<T: PetriVmmBackend>(
    resolver: &petri::ArtifactResolver<'_>,
) -> Option<OpenhclTmkArtifacts<T>> {
    let arch = MachineArch::host();
    if MachineArch::host() != MachineArch::X86_64 {
        // TODO: aarch64 currently hangs on hyperv, fix
        // not supported with OpenVMM
        return None;
    }

    let (igvm_path, tmk_vmm, tmk) = resolve_paravisor_tmk_artifacts(resolver, arch);

    let vm = petri::PetriVmArtifacts::new(
        resolver,
        petri::Firmware::OpenhclUefi {
            guest: petri::UefiGuest::None,
            isolation: None,
            igvm_path,
            uefi_config: Default::default(),
            openhcl_config: Default::default(),
        },
        arch,
        false,
    )?;
    Some(OpenhclTmkArtifacts { vm, tmk_vmm, tmk })
}

fn openvmm_openhcl_tmks(
    params: petri::PetriTestParams<'_>,
    artifacts: OpenhclTmkArtifacts<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    DefaultPool::run_with(async |driver| {
        let logger = params.logger.clone();
        let mut vm = petri::PetriVmBuilder::new(params, artifacts.vm, &driver)?
            .with_openhcl_command_line(OPENHCL_COMMAND_LINE)
            .with_expect_no_boot_event()
            .with_openhcl_agent_file("tmk_vmm", artifacts.tmk_vmm)
            .with_openhcl_agent_file("simple_tmk", artifacts.tmk)
            .with_processor_topology(ProcessorTopology {
                vp_count: 1,
                ..Default::default()
            })
            // TODO: remove once the TMK VMM initializes memory properly.
            // and unify test functions into one generic function
            .modify_backend(|b| b.with_allow_early_vtl0_access(true))
            .run_without_agent()
            .await?;

        tracing::info!("started vm");
        openhcl_tmks_inner(&driver, logger, &mut vm).await?;

        Ok(())
    })
}

#[cfg(windows)]
mod hyperv {
    use crate::OPENHCL_COMMAND_LINE;
    use crate::OpenhclTmkArtifacts;
    use crate::openhcl_tmks_inner;
    use crate::resolve_openhcl_tmks;
    use pal_async::DefaultPool;
    use petri::ProcessorTopology;
    use petri::hyperv::HyperVPetriBackend;

    fn hyperv_openhcl_tmks(
        params: petri::PetriTestParams<'_>,
        artifacts: OpenhclTmkArtifacts<HyperVPetriBackend>,
    ) -> anyhow::Result<()> {
        DefaultPool::run_with(async |driver| {
            let logger = params.logger.clone();
            let mut vm = petri::PetriVmBuilder::new(params, artifacts.vm, &driver)?
                .with_openhcl_command_line(OPENHCL_COMMAND_LINE)
                .with_expect_no_boot_event()
                .with_openhcl_agent_file("tmk_vmm", artifacts.tmk_vmm)
                .with_openhcl_agent_file("simple_tmk", artifacts.tmk)
                .with_processor_topology(ProcessorTopology {
                    vp_count: 1,
                    ..Default::default()
                })
                .run_without_agent()
                .await?;

            tracing::info!("started vm");
            openhcl_tmks_inner(&driver, logger, &mut vm).await?;

            Ok(())
        })
    }

    petri::test!(
        hyperv_openhcl_tmks,
        resolve_openhcl_tmks::<HyperVPetriBackend>
    );
}
