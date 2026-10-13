// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TEST ONLY (not for merge): evaluate enabling the guest watchdog by default
//! for every UEFI VM. These tests gather evidence about how guests react to the
//! device and its ACPI WDAT table, especially on aarch64.

use crate::multiarch::cmd;
use petri::PetriVm;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri_artifacts_common::tags::MachineArch;
use std::time::Duration;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_with;

/// Programs the guest watchdog directly through its I/O ports (x86 only),
/// performing the same register writes the firmware's WDAT describes.
const ARM_WATCHDOG_PY: &str = r#"
import ctypes, mmap, struct
libc = ctypes.CDLL(None, use_errno=True)
assert libc.iopl(3) == 0, "iopl failed"
def outl(port, val):
    # mov dx, port ; mov eax, val ; out dx, eax ; ret
    code = b"\x66\xba" + struct.pack("<H", port) + b"\xb8" + struct.pack("<I", val) + b"\xef\xc3"
    buf = mmap.mmap(-1, 4096, prot=mmap.PROT_READ | mmap.PROT_WRITE | mmap.PROT_EXEC)
    buf.write(code)
    ctypes.CFUNCTYPE(None)(ctypes.addressof(ctypes.c_char.from_buffer(buf)))()
outl(0x30, 0x29); outl(0x34, 20)   # COUNT = 20 seconds
outl(0x30, 0x27); outl(0x34, 0x3)  # CONFIG = configured | enabled
"#;

async fn log_watchdog_state<T: PetriVmmBackend>(vm: &PetriVm<T>, label: &str) {
    for path in ["guest-watchdog", "chipset/pio/0x30-0x37"] {
        match vm.inspect_vmm(path).await {
            Ok(node) => tracing::info!("WATCHDOG [{label}] {path}: {node:?}"),
            Err(e) => tracing::info!("WATCHDOG [{label}] {path}: inspect failed: {e:#}"),
        }
    }
}

async fn log_guest_command(agent: &PipetteClient, script: &str) -> anyhow::Result<()> {
    let sh = agent.unix_shell();
    let script = format!("{script}; true");
    let out = cmd!(sh, "sudo")
        .args(["sh", "-c", &script])
        .output()
        .await?;
    tracing::info!(
        "WATCHDOG $ {script}\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Checks that the firmware publishes a WDAT and records how the Linux guest
/// handles it. On x64, also arms the device and verifies the VM halts with a
/// watchdog timeout.
async fn check_linux_guest(
    mut vm: PetriVm<OpenVmmPetriBackend>,
    agent: PipetteClient,
) -> anyhow::Result<()> {
    log_watchdog_state(&vm, "after boot").await;

    let wdat = agent
        .unix_shell()
        .read_file_raw("/sys/firmware/acpi/tables/WDAT")
        .await?;
    assert!(wdat.starts_with(b"WDAT"), "firmware did not publish a WDAT");

    log_guest_command(
        &agent,
        "uname -a; od -An -tx1 -v /sys/firmware/acpi/tables/WDAT; \
         dmesg | grep -i -E 'wdat|watchdog'; ls -l /sys/class/watchdog/ /dev/watchdog* 2>&1; \
         grep -rn wdat /lib/modprobe.d /etc/modprobe.d 2>&1 | head; modinfo -n wdat_wdt 2>&1",
    )
    .await?;

    if vm.arch() == MachineArch::X86_64 {
        agent
            .write_file("/tmp/arm_watchdog.py", ARM_WATCHDOG_PY.as_bytes())
            .await?;
        log_guest_command(&agent, "python3 /tmp/arm_watchdog.py").await?;
        log_watchdog_state(&vm, "after arm").await;
        let halt = vm.wait_for_halt().await?;
        tracing::info!("WATCHDOG halt after arming: {halt:?}");
        assert!(
            halt.detail.contains("Watchdog"),
            "expected a watchdog halt, got {halt:?}"
        );
        vm.teardown().await?;
    } else {
        log_guest_command(&agent, "sleep 30").await?;
        log_watchdog_state(&vm, "after 30s").await;
        agent.power_off().await?;
        vm.wait_for_clean_teardown().await?;
    }
    Ok(())
}

/// Linux UEFI guests with the guest watchdog on by default.
#[openvmm_test(
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn guest_watchdog_default_on_linux(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    check_linux_guest(vm, agent).await
}

/// Same as [`guest_watchdog_default_on_linux`] for the aarch64 TCG incubator
/// pass, configured like `boot_no_hv_uefi_aarch64_tcg`.
#[cfg(target_os = "linux")]
#[openvmm_test(uefi_aarch64(vhd(ubuntu_2404_server_aarch64)))]
async fn guest_watchdog_default_on_linux_aarch64_tcg(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_no_hv()
        .with_boot_device_type(petri::BootDeviceType::PcieNvme)
        .with_default_boot_always_attempt(true)
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 3))
        .run()
        .await?;
    check_linux_guest(vm, agent).await
}

/// Windows UEFI guests with the guest watchdog on by default: record whether
/// Windows discovers the device, and its PnP status.
#[vmm_test(
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))
)]
async fn guest_watchdog_default_on_windows<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    let shell = agent.windows_shell();
    let out = cmd!(shell, "powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_PnPEntity | Where-Object { $_.Name -match 'atchdog' -or $_.DeviceID -match 'WDAT|WDT' } | Format-List Name,DeviceID,Status,ConfigManagerErrorCode; exit 0",
        ])
        .output()
        .await?;
    tracing::info!(
        "WATCHDOG windows PnP:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    log_watchdog_state(&vm, "windows after boot").await;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Windows UEFI guests without an agent: verify the VM is not reset over
/// several minutes (longer than the 5 minute timeout Windows programs).
#[vmm_test_with(
    noagent,
    configs(
        openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
        openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    )
)]
async fn guest_watchdog_default_on_windows_idle<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    for i in 1..=16 {
        let halted = mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(30))
            .until_cancelled(vm.wait_for_halt())
            .await;
        if let Ok(halt) = halted {
            let halt = halt?;
            log_watchdog_state(&vm, "after halt").await;
            anyhow::bail!("unexpected halt after ~{}s: {halt:?}", i * 30);
        }
        log_watchdog_state(&vm, &format!("t+{}s", i * 30)).await;
    }
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
