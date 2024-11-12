// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Generation 2 UEFI x86_64 guests with OpenHCL.

use petri::PetriVmConfig;
use vmm_core_defs::HaltReason;
use vmm_test_macros::vmm_test;

async fn nvme_relay_test_core(
    config: PetriVmConfig,
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

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
#[vmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "").await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the shared pool override to test the shared pool dma path.
#[vmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_shared_pool(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1").await
}

/// Boot the UEFI firmware, with a VTL2 range automatically configured by
/// hvlite.
#[vmm_test(openhcl_uefi_x64(none))]
async fn auto_vtl2_range(config: PetriVmConfig) -> Result<(), anyhow::Error> {
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
