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

    let devices = vm.inspect_openhcl("vm/nvme/devices", None, None).await?;
    tracing::info!(devices = %devices.json(), "NVMe devices");

    let devices: serde_json::Value = serde_json::from_str(&format!("{}", devices.json()))?;

    /*
    {
        "718b:00:00.0": {
            "driver": {
                "driver": {
                    "admin": {
                        ...
                    },
                    "bounce_buffer": false,
                    "device": {
                        "dma_client": {
                            "backing": {
                                "type": "locked_memory"
                            },
                            "params": {
                                "allocation_visibility": "private",
                                "device_name": "nvme_718b:00:00.0",
                                "lower_vtl_policy": "any",
                                "persistent_allocations": false
                            }
                        },
                        "interrupts": {
                            "0": {
                                "target_cpu": 0
                            }
                        },
                        "pci_id": "718b:00:00.0"
                    },
                    "device_id": "718b:00:00.0",
                    "identify": {
                        ...
                    },
                    "io": {
                        ...
                    },
                    "io_issuers": {
                        ...
                    },
                    "max_io_queues": 1,
                    "nvme_keepalive": false,
                    "qsize": 64,
                    "registers": {
                        ...
                    }
                },
                "pci_id": "718b:00:00.0"
            },
            "pci_id": "718b:00:00.0",
            "save_restore_supported": false,
            "vp_count": 1
        }
    }
    */

    // If just one device is returned, then this will be a `Value::Object`, where the
    // key is the single PCI ID of the device.
    //
    // TODO (future PR): Fix this up with support for multiple devices when this code is used
    // in more complicated tests.
    let found_device_id = devices
        .as_object()
        .expect("devices object")
        .keys()
        .next()
        .expect("device id");

    // The PCI id is generated from the VMBUS instance guid for vpci devices.
    // See `PARAVISOR_BOOT_NVME_INSTANCE`.
    assert_eq!(found_device_id, "718b:00:00.0");
    if let Some(props) = &props {
        assert_eq!(
            devices[found_device_id]["driver"]["driver"]["qsize"]
                .as_u64()
                .expect("qsize"),
            props.qsize
        );
        assert_eq!(
            devices[found_device_id]["driver"]["driver"]["nvme_keepalive"]
                .as_bool()
                .expect("nvme_keepalive"),
            props.nvme_keepalive
        );
        assert_eq!(
            devices[found_device_id]["save_restore_supported"]
                .as_bool()
                .expect("save_restore_supported"),
            props.save_restore_supported
        );
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2504_server_x64)))]
async fn nvme_relay(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "", None).await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the shared pool override to test the shared pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2504_server_x64)))]
async fn nvme_relay_shared_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    nvme_relay_test_core(config, "OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1", None).await
}

/// Test an OpenHCL uefi VM with a NVME disk assigned to VTL2 that boots
/// linux, with vmbus relay. This should expose a disk to VTL0 via vmbus.
///
/// Use the private pool override to test the private pool dma path.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2504_server_x64)))]
async fn nvme_relay_private_pool(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    // Number of pages to reserve as a private pool.
    nvme_relay_test_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512",
        Some(ExpectedNvmeDeviceProperties {
            save_restore_supported: true,
            qsize: 256, // private pool should allow contiguous allocations.
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
