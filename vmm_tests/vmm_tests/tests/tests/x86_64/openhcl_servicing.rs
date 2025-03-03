// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 OpenHCL servicing.

use petri::openvmm::PetriVmConfigOpenVmm;
use petri::OpenHclServicingFlags;
use petri::ResolvedArtifact;
#[cfg(guest_arch = "x86_64")]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::RELEASE_2_4_11_LINUX_DIRECT_X64;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

async fn openhcl_servicing_core(
    config: PetriVmConfigOpenVmm,
    openhcl_cmdline: &str,
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .run()
        .await?;

    agent.ping().await?;

    // Test that inspect serialization works with the old version.
    vm.test_inspect_openhcl().await?;

    vm.restart_openhcl(new_openhcl, flags).await?;

    agent.ping().await?;

    // Test that inspect serialization works with the new version.
    vm.test_inspect_openhcl().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test servicing an OpenHCL VM from the current version to itself.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(config, "", igvm_file, OpenHclServicingFlags::default()).await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with VF keepalive support.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing_keepalive(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512",
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
        },
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64, RELEASE_2_4_11_LINUX_DIRECT_X64])]
async fn test_servicing_x64_linux_direct_2411_to_latest(
    config: PetriVmConfigOpenVmm,
    (latest_igvm, release_igvm): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    ),
) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(
        config.with_custom_openhcl(release_igvm),
        "",
        latest_igvm,
        OpenHclServicingFlags::default(),
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing_shutdown_ic(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.with_vmbus_redirect().run().await?;
    agent.ping().await?;
    let shutdown_ic = vm.wait_for_enlightened_shutdown_ready().await?;
    vm.restart_openhcl(igvm_file, OpenHclServicingFlags::default())
        .await?;
    // VTL2 will disconnect and then reconnect the shutdown IC across a servicing event.
    tracing::info!("waiting for shutdown IC to close");
    shutdown_ic.await.unwrap_err();
    vm.wait_for_enlightened_shutdown_ready().await?;
    vm.send_enlightened_shutdown(petri::ShutdownKind::Shutdown)
        .await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

// TODO: add tests with guest workloads while doing servicing.
