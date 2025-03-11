// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use petri::openvmm::PetriVmConfigOpenVmm;
use petri::PetriVmConfig;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[openvmm_test(uefi_x64(none), openhcl_uefi_x64(none), uefi_aarch64(none))]
async fn frontpage(config: PetriVmConfigOpenVmm) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

// TODO: reorganize tests based on VMM
/// Basic boot test.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_pcat_x64(vhd(ubuntu_2204_server_x64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn boot(config: Box<dyn PetriVmConfig>) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}
