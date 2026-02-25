// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 guests.

mod openhcl_linux_direct;
mod openhcl_uefi;

use anyhow::Context;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::VpciDeviceConfig;
use net_backend_resources::mac_address::MacAddress;
use net_backend_resources::null::NullHandle;
use nvme_resources::NvmeControllerHandle;
use petri::ApicMode;
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

/// Basic boot test with the VTL 0 alias map.
// TODO: Remove once #73 is fixed.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
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

/// Basic boot tests with TPM enabled.
#[openvmm_test(
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot_with_tpm(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let config = config.modify_backend(|b| b.with_tpm());

    let (vm, agent) = match os_flavor {
        OsFlavor::Windows => config.run().await?,
        OsFlavor::Linux => {
            config
                .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
                // TODO: this shouldn't be needed once with_tpm() is
                // backend-agnostic.
                .with_expect_reset()
                .run()
                .await?
        }
        _ => unreachable!(),
    };

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// TODO: Test disabled because AK cert renewal with non-hardware isolation is
// disabled.
// /// Test AK cert is persistent across boots on Linux.
// // TODO: Add in-guest TPM tests for Windows as we currently
// // do have an easy way to interact with TPM without a private
// // or custom tool.
// #[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)))]
// async fn tpm_ak_cert_persisted(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
//     let config = config
//         .with_tpm()
//         .with_tpm_state_persistence()
//         .with_igvm_attest_test_config(
//             get_resources::ged::IgvmAttestTestConfig::AkCertPersistentAcrossBoot,
//         );

//     // First boot - AK cert request will be served by GED
//     let mut vm = config.run_with_lazy_pipette().await?;
//     // Workaround to https://github.com/microsoft/openvmm/issues/379
//     assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);

//     // Second boot - Ak cert request will be bypassed by GED
//     vm.reset().await?;
//     let agent = vm.wait_for_agent().await?;
//     vm.wait_for_successful_boot_event().await?;

//     // Use the python script to read AK cert from TPM nv index
//     // and verify that the AK cert preserves across boot.
//     // TODO: Replace the script with tpm2-tools
//     const TEST_FILE: &str = "tpm.py";
//     const TEST_CONTENT: &str = include_str!("../../test_data/tpm.py");

//     agent.write_file(TEST_FILE, TEST_CONTENT.as_bytes()).await?;
//     assert_eq!(agent.read_file(TEST_FILE).await?, TEST_CONTENT.as_bytes());

//     let sh = agent.unix_shell();
//     let output = cmd!(sh, "python3 tpm.py").read().await?;

//     // Check if the content preserves as expected
//     assert!(output.contains("succeeded"));

//     agent.power_off().await?;
//     assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
//     Ok(())
// }

// TODO: Test disabled because AK cert renewal with non-hardware isolation is
// disabled.
// /// Test AK cert retry logic on Linux.
// // TODO: Add in-guest TPM tests for Windows as we currently
// // do have an easy way to interact with TPM without a private
// // or custom tool.
// #[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)))]
// async fn tpm_ak_cert_retry(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
//     let config = config
//         .with_tpm()
//         .with_tpm_state_persistence()
//         .with_igvm_attest_test_config(
//             get_resources::ged::IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
//         );

//     // First boot - expect no AK cert from GED
//     let mut vm = config.run_with_lazy_pipette().await?;
//     // Workaround to https://github.com/microsoft/openvmm/issues/379
//     assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);

//     // Second boot - except get AK cert from GED on the second attempts
//     vm.reset().await?;
//     let agent = vm.wait_for_agent().await?;
//     vm.wait_for_successful_boot_event().await?;

//     // Use the python script to read AK cert from TPM nv index
//     // and verify that the AK cert preserves across boot.
//     // TODO: Replace the script with tpm2-tools
//     const TEST_FILE: &str = "tpm.py";
//     const TEST_CONTENT: &str = include_str!("../../test_data/tpm.py");

//     agent.write_file(TEST_FILE, TEST_CONTENT.as_bytes()).await?;
//     assert_eq!(agent.read_file(TEST_FILE).await?, TEST_CONTENT.as_bytes());

//     // The first AK cert request made during boot is expected to
//     // get invalid response from GED such that no data is set
//     // to nv index. The script should return failure. Also, the nv
//     // read made by the script is expected to trigger another AK cert
//     // request.
//     let sh = agent.unix_shell();
//     let output = cmd!(sh, "python3 tpm.py").read().await?;

//     // Check if there is no content yet
//     assert!(!output.contains("succeeded"));

//     // Run the script again to test if the AK cert triggered by nv read
//     // succeeds and the data is written into the nv index.
//     let sh = agent.unix_shell();
//     let output = cmd!(sh, "python3 tpm.py").read().await?;

//     // Check if the content is now available
//     assert!(output.contains("succeeded"));

//     agent.power_off().await?;
//     assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
//     Ok(())
// }

/// Basic VBS boot test with TPM enabled.
#[openvmm_test_no_agent(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64))
)]
async fn vbs_boot_with_tpm(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let config = config.modify_backend(|b| b.with_tpm());

    let mut vm = match os_flavor {
        OsFlavor::Windows => config.run_without_agent().await?,
        OsFlavor::Linux => {
            config
                .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
                // TODO: this shouldn't be needed once with_tpm() is
                // backend-agnostic.
                .with_expect_reset()
                .run_without_agent()
                .await?
        }
        _ => unreachable!(),
    };

    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic VTL 2 pipette functionality test.
#[openvmm_test(openhcl_linux_direct_x64)]
async fn vtl2_pipette(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (mut vm, agent) = config.run().await?;

    let vtl2_agent = vm.wait_for_vtl2_agent().await?;
    let sh = vtl2_agent.unix_shell();
    let output = cmd!(sh, "ps").read().await?;
    assert!(output.contains("openvmm_hcl vm"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot Linux and have it dump MTRR related output.
#[openvmm_test(linux_direct_x64, openhcl_linux_direct_x64)]
async fn mtrrs(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.run().await?;

    let sh = agent.unix_shell();
    // Read /proc before dmesg, as reading it can trigger more messages.
    let mtrr_output = sh.read_file("/proc/mtrr").await?;
    let dmesg_output = cmd!(sh, "dmesg").read().await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    // Validate that output does not contain any MTRR-related errors.
    // If all MTRR registers are zero we get this message.
    assert!(!dmesg_output.contains("CPU MTRRs all blank - virtualized system"));
    // If the BSP and APs have different MTRR values we get "your CPUs had inconsistent (fixed MTRR/variable MTRR/MTRRdefType) settings" messages.
    assert!(!dmesg_output.contains("your CPUs had inconsistent"));
    // If we misread the physical address size we can end up computing incorrect MTRR masks
    assert!(!dmesg_output.contains("your BIOS has configured an incorrect mask"));
    // The Linux kernel may also output general 'something is not right' messages, check for those too.
    assert!(!dmesg_output.contains("probably your BIOS does not setup all CPUs"));
    assert!(!dmesg_output.contains("corrected configuration"));
    assert!(!dmesg_output.contains("BIOS bug"));

    // Validate that the output contains MTRR enablement messages.
    //
    // TODO: these are only output if DEBUG is enabled for Linux's mtrr.c, which
    // it no longer is by default in newer kernel versions.
    // assert!(mtrr_output.contains("default type: uncachable"));
    // assert!(mtrr_output.contains("fixed ranges enabled"));
    // assert!(mtrr_output.contains("variable ranges enabled"));
    assert!(
        mtrr_output
            .contains("reg00: base=0x000000000 (    0MB), size=  128MB, count=1: write-back")
    );
    assert!(
        mtrr_output
            .contains("reg01: base=0x008000000 (  128MB), size= 4096MB, count=1: write-back")
    );

    Ok(())
}

/// Boot with vmbus redirection and shut down.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn vmbus_redirect(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.with_vmbus_redirect(true).run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a battery and check the OS-reported capacity.
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64)),
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
) -> PetriVmBuilder<T> {
    config.with_processor_topology({
        ProcessorTopology {
            vp_count: proc_count,
            // Sidecar will start one VP per socket. For this test, use just one
            // socket.
            vps_per_socket: Some(proc_count),
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
    let mut vm = configure_for_sidecar(config, proc_count)
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
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn sidecar_boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (vm, agent) = configure_for_sidecar(config, 8).run().await?;
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
