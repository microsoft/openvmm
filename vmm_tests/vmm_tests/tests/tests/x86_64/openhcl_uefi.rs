// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Generation 2 UEFI x86_64 guests with OpenHCL.

use petri::openvmm::PetriVmConfigOpenVmm;
use petri::OpenHclServicingFlags;
use petri::ResolvedArtifact;
#[cfg(guest_arch = "x86_64")]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

async fn nvme_relay_test_core(
    config: PetriVmConfigOpenVmm,
    openhcl_cmdline: &str,
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .with_vmbus_redirect()
        .with_single_processor()
        .run()
        .await?;

    vm.wait_for_successful_boot_event().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Servicing tests with NVMe devices attached.
async fn nvme_relay_servicing_core(
    config: PetriVmConfigOpenVmm,
    openhcl_cmdline: &str,
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .with_vmbus_redirect()
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

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "").await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the shared pool override to test the shared pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_shared_pool(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1").await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the private pool override to test the private pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_private_pool(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    // Number of pages to reserve as a private pool.
    nvme_relay_test_core(config, "OPENHCL_ENABLE_VTL2_GPA_POOL=1024").await
}

/// Servicing test of an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
/// Use the private pool override to test the private pool dma path.
/// Pass 'keepalive' servicing flag which impacts NVMe save/restore.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64))[LATEST_STANDARD_X64])]
async fn nvme_keepalive(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    nvme_relay_servicing_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=1024",
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
        },
    )
    .await
}

/// Boot the UEFI firmware, with a VTL2 range automatically configured by
/// hvlite.
#[openvmm_test(openhcl_uefi_x64(none))]
async fn auto_vtl2_range(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let mut vm = config
        .with_vtl2_relocation_mode(hvlite_defs::config::Vtl2BaseAddressType::MemoryLayout {
            size: None,
        })
        .run_without_agent()
        .await?;

    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}
