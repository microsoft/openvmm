// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 guests.

mod openhcl_linux_direct;
mod openhcl_uefi;
mod storage;

use anyhow::Context;
use net_backend_resources::mac_address::MacAddress;
use net_backend_resources::null::NullHandle;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
use petri::ApicMode;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

/// Basic boot test with the VTL 0 alias map.
// TODO: Remove once #73 is fixed.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn boot_alias_map(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_vtl0_alias_map())
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a battery and check the OS-reported capacity.
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2504_server_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn battery_capacity(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config.modify_backend(|b| b.with_battery()).run().await?;

    let output = match os_flavor {
        OsFlavor::Linux => {
            let sh = agent.unix_shell();
            cmd!(
                sh,
                "grep POWER_SUPPLY_CAPACITY= /sys/class/power_supply/BAT1/uevent"
            )
            .read()
            .await?
            .replace("POWER_SUPPLY_CAPACITY=", "")
        }
        OsFlavor::Windows => {
            let sh = agent.windows_shell();
            cmd!(
                sh,
                "powershell.exe -NoExit -Command (Get-WmiObject Win32_Battery).EstimatedChargeRemaining"
            )
            .read()
            .await?
            .replace("\r\nPS C:\\>", "")
            .trim()
            .to_string()
        }
        _ => unreachable!(),
    };

    let guest_capacity: i32 = output.parse().expect("Failed to parse battery capacity");
    assert_eq!(guest_capacity, 95, "Output did not match expected capacity");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

fn configure_for_sidecar<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    proc_count: u32,
    node_count: u32,
) -> PetriVmBuilder<T> {
    config.with_processor_topology({
        ProcessorTopology {
            vp_count: proc_count,
            vps_per_socket: Some(proc_count / node_count),
            enable_smt: Some(false),
            // Sidecar currently requires x2APIC.
            apic_mode: Some(ApicMode::X2apicSupported),
        }
    })
}

// Use UEFI so that the guest doesn't access the other APs, causing hot adds
// into VTL2 Linux.
//
// Sidecar isn't supported on aarch64 yet.
#[vmm_test_no_agent(openvmm_openhcl_uefi_x64(none), hyperv_openhcl_uefi_x64(none))]
async fn sidecar_aps_unused<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let proc_count = 4;
    let mut vm = configure_for_sidecar(config, proc_count, 1)
        .with_uefi_frontpage(true)
        .run_without_agent()
        .await?;

    let agent = vm.wait_for_vtl2_agent().await?;
    let sh = agent.unix_shell();

    // Ensure the APs haven't been started into Linux.
    //
    // CPU 0 doesn't usually have an online file on x86_64.
    for cpu in 1..proc_count {
        let online = sh
            .read_file(format!("/sys/bus/cpu/devices/cpu{cpu}/online"))
            .await?
            .trim()
            .parse::<u8>()
            .context("failed to parse online file")?
            != 0;
        assert!(!online, "cpu {cpu} is online");
    }

    // No way to shut down cleanly, currently.
    tracing::info!("dropping VM");
    Ok(())
}

#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn sidecar_boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (vm, agent) = configure_for_sidecar(config, 8, 2).run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[openvmm_test(openhcl_linux_direct_x64)]
async fn vpci_filter(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let nvme_guid = guid::guid!("78fc4861-29bf-408d-88b7-24199de560d1");
    let virtio_guid = guid::guid!("382a9da7-a7d8-44a5-9644-be3785bceda6");

    // Add an NVMe controller and a Virtio network controller. Only the NVMe
    // controller should be allowed by OpenHCL.
    let (vm, agent) = config
        .with_openhcl_command_line("OPENHCL_ENABLE_VPCI_RELAY=1")
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([
                    VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: nvme_guid,
                        resource: NvmeControllerHandle {
                            subsystem_id: nvme_guid,
                            msix_count: 1,
                            max_io_queues: 1,
                            namespaces: Vec::new(),
                        }
                        .into_resource(),
                    },
                    VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: virtio_guid,
                        resource: VirtioPciDeviceHandle(
                            VirtioNetHandle {
                                max_queues: None,
                                mac_address: MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x12]),
                                endpoint: NullHandle.into_resource(),
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    },
                ])
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();
    let lspci_output = cmd!(sh, "lspci").read().await?;
    let devices = lspci_output
        .lines()
        .map(|line| line.trim().split_once(' ').ok_or_else(|| line.trim()))
        .collect::<Vec<_>>();

    // The virtio device should not have made it through, but the NVMe
    // controller should be there.
    assert_eq!(devices, vec![Ok(("00:00.0", "Class 0108: 1414:00a9"))]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
