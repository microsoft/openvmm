// Copyright (C) Microsoft Corporation. All rights reserved.

//! Integration tests for x86_64 guests.

mod openhcl_linux_direct;
mod openhcl_uefi;

use anyhow::Context;
use petri::pipette::cmd;
use petri::PetriVmConfig;
use petri::ShutdownKind;
use petri::SIZE_1_GB;
use petri_artifacts_common::tags::OsFlavor;
use vmm_core_defs::HaltReason;
use vmm_test_macros::vmm_test;

/// Basic boot test with no agent for unsupported guests.
#[vmm_test(pcat_x64(vhd(freebsd_13_2_x64)), pcat_x64(iso(freebsd_13_2_x64)))]
async fn boot_no_agent(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
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
    pcat_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot(config: PetriVmConfig) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic boot test with the VTL 0 alias map.
// TODO: Remove once #912 is fixed.
#[vmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn boot_alias_map(config: PetriVmConfig) -> anyhow::Result<()> {
    let (vm, agent) = config.with_vtl0_alias_map().run().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic VBS boot test.
#[vmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64))
)]
async fn vbs_boot(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic VBS boot test with a single VP.
#[vmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64))
)]
async fn vbs_boot_single_proc(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.with_single_processor().run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic VBS boot test with TPM enabled.
// TODO: Reenable the linux test after the reboot failure is resolved.
#[vmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    // openhcl_uefi_vbs_x64(vhd(ubuntu_2204_server_x64))
)]
async fn vbs_boot_with_tpm(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.with_tpm().run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Validate we can reboot a VM and reconnect to pipette.
// TODO: Reenable interesting guests once #523 is fixed.
#[vmm_test(
    linux_direct_x64,
    openhcl_linux_direct_x64,
    // openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    // openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    // uefi_x64(vhd(ubuntu_2204_server_x64)),
    // pcat_x64(vhd(ubuntu_2204_server_x64))
)]
async fn reboot(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.run().await?;

    agent.ping().await?;

    agent.reboot().await?;
    assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);
    vm.reset().await?;

    let agent = vm.wait_for_agent().await?;

    agent.ping().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Basic VBS reboot test.
#[vmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64))
)]
async fn vbs_reboot(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Reboot).await?;
    assert_eq!(vm.wait_for_halt().await?, HaltReason::Reset);
    vm.reset().await?;
    vm.wait_for_successful_boot_event().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Basic VTL 2 pipette functionality test.
#[vmm_test(openhcl_linux_direct_x64)]
async fn vtl2_pipette(config: PetriVmConfig) -> anyhow::Result<()> {
    let (mut vm, agent) = config.run().await?;

    let vtl2_agent = vm.wait_for_vtl2_agent().await?;
    let sh = vtl2_agent.unix_shell();
    let output = cmd!(sh, "ps").read().await?;
    assert!(output.contains("underhill vm"));

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test(uefi_x64(none), openhcl_uefi_x64(none))]
async fn frontpage(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Boot through the UEFI firmware with a TPM, it will shut itself down after booting.
// We don't build the TPM on Windows, since building OpenSSL on Windows is hard.
// TODO: Add checks that the TPM is visible to the guest once we have a way to do that.
// For now this will just test that the TPM is wired up correctly, which could result
// in errors with resolving internal resources.
#[cfg_attr(windows, vmm_test(openhcl_uefi_x64(none)))]
#[cfg_attr(not(windows), vmm_test(uefi_x64(none), openhcl_uefi_x64(none)))]
async fn frontpage_tpm(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.with_tpm().run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Boot our guest-test UEFI image, which will run some tests,
/// and then purposefully triple fault itself via an expiring
/// watchdog timer.
#[vmm_test(uefi_x64(guest_test_uefi_x64), openhcl_uefi_x64(guest_test_uefi_x64))]
async fn guest_test_uefi(config: PetriVmConfig) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .run_without_agent()
        .await?;
    // No boot event check, UEFI watchdog gets fired before ExitBootServices
    assert!(matches!(
        vm.wait_for_teardown().await?,
        HaltReason::TripleFault { .. }
    ));
    Ok(())
}

/// Boot Linux and have it write the visible memory size.
#[vmm_test(linux_direct_x64)]
async fn five_gb(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    let configured_size = 5 * SIZE_1_GB;
    let expected_size = configured_size - configured_size / 10; // 10% buffer; TODO-figure out where this goes

    let (vm, agent) = config
        .with_custom_config(|c| c.memory.mem_size = configured_size)
        .run()
        .await?;

    // Validate that the RAM size is appropriate.
    // Skip the first 9 characters, which are "MemTotal:", and the last two,
    // which are the units.
    let output = agent.unix_shell().read_file("/proc/meminfo").await?;
    let memtotal_line = output
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .context("coulnd't find memtotal")?;
    let size_kb: u64 = memtotal_line
        .strip_suffix("kB")
        .context("memtotal units should be in kB")?
        .trim()
        .parse()
        .context("couldn't parse size")?;
    assert!(
        size_kb * 1024 >= expected_size,
        "memory size {} >= {}",
        size_kb * 1024,
        expected_size
    );

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test transferring a file to the guest.
#[vmm_test(
    linux_direct_x64,
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn file_transfer_test(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    const TEST_CONTENT: &str = "hello world!";
    const FILE_NAME: &str = "test.txt";

    let (vm, agent) = config.run().await?;

    agent.write_file(FILE_NAME, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(FILE_NAME).await?, TEST_CONTENT.as_bytes());

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Boot Linux and have it dump MTRR related output.
#[vmm_test(linux_direct_x64, openhcl_linux_direct_x64)]
async fn mtrrs(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.run().await?;

    let sh = agent.unix_shell();
    // Read /proc before dmesg, as reading it can trigger more messages.
    let mtrr_output = sh.read_file("/proc/mtrr").await?;
    let dmesg_output = cmd!(sh, "dmesg").read().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

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
    assert!(mtrr_output
        .contains("reg00: base=0x000000000 (    0MB), size=  128MB, count=1: write-back"));
    assert!(mtrr_output
        .contains("reg01: base=0x008000000 (  128MB), size= 4096MB, count=1: write-back"));

    Ok(())
}

/// Boot with vmbus redirection and shut down.
#[vmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))
)]
async fn vmbus_redirect(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.with_vmbus_redirect().run().await?;
    vm.wait_for_successful_boot_event().await?;
    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

/// Boot with a battery and check the OS-reported capacity.
#[vmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2204_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2204_server_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn battery_capacity(config: PetriVmConfig) -> Result<(), anyhow::Error> {
    let os_flavor = config.os_flavor();
    let (mut vm, agent) = config.with_battery().run().await?;
    vm.wait_for_successful_boot_event().await?;

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
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}