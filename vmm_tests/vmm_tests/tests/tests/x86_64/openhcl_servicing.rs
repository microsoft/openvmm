// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 OpenHCL servicing.

use petri::openvmm::PetriVmConfigOpenVmm;
use petri::ArtifactHandle;
use petri::OpenHclServicingFlags;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

async fn openhcl_servicing_core(
    config: PetriVmConfigOpenVmm,
    new_openhcl: ArtifactHandle<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config.run().await?;

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
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_servicing(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(
        config,
        LATEST_LINUX_DIRECT_TEST_X64,
        OpenHclServicingFlags::default(),
    )
    .await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with VF keepalive support.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn openhcl_servicing_keepalive(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(
        config,
        LATEST_LINUX_DIRECT_TEST_X64,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
        },
    )
    .await
}

// TODO: add tests with guest workloads while doing servicing.
// TODO: add tests from previous release branch to current.
