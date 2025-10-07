// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Generation 2 UEFI x86_64 guests with OpenHCL.

use anyhow::Context;
use futures::StreamExt;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;
use vmm_test_macros::vmm_test_no_agent;

struct ExpectedNvmeDeviceProperties {
    save_restore_supported: bool,
    qsize: u64,
    nvme_keepalive: bool,
}

/// Helper to run a scenario where we boot an OpenHCL UEFI VM with a NVME
/// disk assigned to VTL2.
///
/// Validates that the VTL2 NVMe driver is working as expected by comparing
/// the inspect properties of the NVMe device against the supplied expected
/// properties.
///
/// If `props` is `None`, then we skip validating the properties. (This is useful
/// at this moment for while we finish developing NVMe keepalive, which is needed
/// to get the devices to work as expected.)
async fn nvme_relay_test_core(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    openhcl_cmdline: &str,
    props: Option<ExpectedNvmeDeviceProperties>,
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

    let devices_node = vm.inspect_openhcl("vm/nvme/devices", None, None).await?;
    tracing::info!(devices = %devices_node.json(), "NVMe devices");
    let inspect::Node::Dir(devices) = devices_node else {
        anyhow::bail!("Not expected: `vm/nvme/devices` is not a directory");
    };

    // Inspect the NVMe devices
    assert!(!devices.is_empty(), "Expected at least one NVMe device");

    if let Some(props) = &props {
        // Validate that we have the expected number of devices.
        assert_eq!(devices.len(), 1, "Expected exactly one NVMe device");
        assert_eq!(
            props.save_restore_supported,
            devices[0]
                .node
                .child_value::<bool>("save_restore_supported")?
        );

        // For now, assume that the first device is just the one we expect.
        // But, [`PARAVISOR_BOOT_NVME_INSTANCE`] contains the PCI instance.
        // Get the guts of the device...
        let device_details = vm
            .inspect_openhcl(
                [
                    "vm/nvme/devices".to_owned(),
                    devices[0].name.clone(),
                    "driver/driver".to_owned(),
                ]
                .join("/")
                .as_str(),
                None,
                None,
            )
            .await?;

        assert_eq!(props.qsize, device_details.child_value::<u64>("qsize")?);

        assert_eq!(
            props.nvme_keepalive,
            device_details.child_value::<bool>("nvme_keepalive")?
        );
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "", None).await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the shared pool override to test the shared pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2204_server_x64)))]
async fn nvme_relay_shared_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1", None).await
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
    nvme_relay_test_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512",
        Some(ExpectedNvmeDeviceProperties {
            save_restore_supported: true,
            qsize: 64,
            nvme_keepalive: false,
        }),
    )
    .await
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
#[vmm_test_no_agent(openvmm_openhcl_uefi_x64(none))]
async fn no_numa_errors<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let vm = config
        .with_openhcl_command_line("OPENHCL_WAIT_FOR_START=1")
        .with_expect_no_boot_event()
        .with_processor_topology(ProcessorTopology {
            vp_count: 2,
            vps_per_socket: Some(1),
            ..Default::default()
        })
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
