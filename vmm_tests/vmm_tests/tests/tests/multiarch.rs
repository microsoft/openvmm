// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use petri::PetriVmConfig;
use vmm_core_defs::HaltReason;
use vmm_test_macros::vmm_test;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test(uefi_x64(none), openhcl_uefi_x64(none), uefi_aarch64(none))]
async fn frontpage(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic boot test.
#[vmm_test(
    linux_direct_x64,
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64)),
    pcat_x64(vhd(ubuntu_2204_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn boot(config: PetriVmConfig) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}
