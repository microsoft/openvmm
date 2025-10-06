// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Generation 2 UEFI x86_64 guests with OpenHCL.

use anyhow::Context;
use futures::StreamExt;
use petri::PetriVmBuilder;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;

async fn nvme_relay_test_core(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    openhcl_cmdline: &str,
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .with_vmbus_redirect(true)
        .with_processor_topology(ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "").await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the shared pool override to test the shared pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_shared_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1").await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the private pool override to test the private pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_private_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    // Number of pages to reserve as a private pool.
    nvme_relay_test_core(config, "OPENHCL_ENABLE_VTL2_GPA_POOL=512").await
}

/// Boot the UEFI firmware, with a VTL2 range automatically configured by
/// hvlite.
#[openvmm_test_no_agent(openhcl_uefi_x64(none))]
async fn auto_vtl2_range(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let vm = config
        .modify_backend(|b| {
            b.with_vtl2_relocation_mode(hvlite_defs::config::Vtl2BaseAddressType::MemoryLayout {
                size: None,
            })
        })
        .run_without_agent()
        .await?;

    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot OpenHCL, and validate that we did not see any numa errors from the
/// kernel parsing the bootloader provided device tree.
///
/// TODO: OpenVMM doesn't support multiple numa nodes yet, but when it does, we
/// should also validate that the kernel gets two different numa nodes.
#[openvmm_test_no_agent(openhcl_uefi_x64(none))]
async fn no_numa_errors(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let vm = config
        .with_openhcl_command_line("OPENHCL_WAIT_FOR_START=1")
        .with_expect_no_boot_event()
        .run_without_agent()
        .await?;

    const BAD_PROP: &str = "OF: NUMA: bad property in memory node";
    const NO_NUMA: &str = "NUMA: No NUMA configuration found";
    const FAKING_NODE: &str = "Faking a node at";

    let mut kmsg = vm.kmsg().await?;

    // Search kmsg and make sure we didn't see any errors from the kernel
    while let Some(data) = kmsg.next().await {
        let data = data.context("reading kmsg")?;
        let msg = kmsg::KmsgParsedEntry::new(&data).unwrap();
        let raw = msg.message.as_raw();
        if raw.contains(BAD_PROP) {
            anyhow::bail!("found bad prop in kmsg");
        }
        if raw.contains(NO_NUMA) {
            anyhow::bail!("found no numa configuration in kmsg");
        }
        if raw.contains(FAKING_NODE) {
            anyhow::bail!("found faking a node in kmsg");
        }
    }

    Ok(())
}
