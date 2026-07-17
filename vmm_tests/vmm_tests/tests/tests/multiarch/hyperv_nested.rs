// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested-virtualization integration test.
//!
//! Enables the Hyper-V role inside an OpenVMM guest and exercises nested L2
//! VMs, including Discrete Device Assignment (DDA) of an emulated NVMe
//! controller.

// TODO: enable for Linux once KVM nested supports reset.
#![cfg(windows)]

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use vmm_test_macros::openvmm_test;

/// Enable the Hyper-V role in a Windows guest, reboot into the hypervisor, and
/// start nested L2 VMs — a plain Gen2 VM and one with a Discrete Device
/// Assignment (DDA) NVMe controller — to confirm nested virtualization works.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64_no_vmbus_prepped)))]
async fn boot_hyperv_role(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_no_vmbus()
        .with_boot_device_type(petri::BootDeviceType::PcieNvme)
        .with_default_boot_always_attempt(true)
        .modify_backend(|b| {
            // Root ports:
            //   s0rc0rp0 — boot NVMe (auto)
            //   s0rc0rp1 — cidata NVMe (auto)
            //   s0rc0rp2 — TCP pipette NIC
            //   s0rc0rp3 — extra NVMe for DDA to L2
            //
            // The guest CPU vendor mirrors the host (the hypervisor passes the
            // physical CPU through), so the emulated IOMMU must match the host
            // vendor: Windows loads its AMD or Intel IOMMU driver based on the
            // CPU vendor and will reject a mismatched DMAR/IVRS table, leaving
            // interrupt remapping unprogrammed and boot I/O interrupts blocked.
            let b = b.with_nested_virt().with_pcie_root_topology(1, 1, 4);
            // Enable hugepages on Windows to improve performance. Linux has THP
            // and doesn't need this.
            //
            // TODO: consider weakening this once soft large pages are
            // supported.
            let b = if cfg!(windows) {
                b.with_hugepages(None)
            } else {
                b
            };
            let b = match petri::requirements::Vendor::host() {
                petri::requirements::Vendor::Amd => b.with_amd_iommu(&["s0rc0"]),
                petri::requirements::Vendor::Intel => b.with_intel_vtd(&["s0rc0"]),
                petri::requirements::Vendor::Arm => {
                    unreachable!("boot_hyperv_role is an x86-only (uefi_x64) test")
                }
            };
            b.with_pcie_nvme(
                "s0rc0rp3",
                guid::guid!("a1b2c3d4-e5f6-7890-abcd-ef0123456789"),
            )
            .with_tcp_pipette_nic("s0rc0rp2")
            .with_custom_config(|c| {
                // Set ACS capability bits on root ports for proper IOMMU
                // group isolation (SV + RR + CR + UF).
                for rc in &mut c.pcie_root_complexes {
                    for port in &mut rc.ports {
                        port.acs_capabilities_supported = Some(0x5D);
                    }
                }
            })
        })
        .run()
        .await?;

    let shell = agent.windows_shell();

    // Install the Hyper-V role and management tools. DISM returns exit code
    // 3010 when a restart is required, which is expected.
    for feature in [
        "Microsoft-Hyper-V",
        "Microsoft-Hyper-V-Management-PowerShell",
    ] {
        let output = cmd!(shell, "dism.exe")
            .args([
                "/online",
                "/enable-feature",
                &format!("/featurename:{feature}"),
                "/all",
                "/norestart",
            ])
            .ignore_status()
            .output()
            .await?;
        let exit_code = output.status.code().context("dism terminated by signal")?;
        anyhow::ensure!(
            exit_code == 0 || exit_code == 3010,
            "dism /enable-feature {feature} failed with exit code {exit_code}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Ensure the hypervisor launches on next boot.
    cmd!(shell, "bcdedit.exe")
        .args(["/set", "hypervisorlaunchtype", "auto"])
        .run()
        .await?;

    // Reboot to start the hypervisor.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    let shell = agent.windows_shell();

    // Run a `powershell.exe -Command <command>` invocation in the guest.
    let pwsh = |command: &str| cmd!(shell, "powershell.exe").args(["-Command", command]);

    // Create and start a small L2 VM to verify nested virtualization works.
    // VMMS may still be initializing its WMI provider after the first boot
    // with Hyper-V enabled, so retry New-VM a few times.
    pwsh(
        "$attempt = 0; while ($attempt -lt 10) { try { New-VM -Name TestL2 -MemoryStartupBytes 64MB -Generation 2 -NoVHD -ErrorAction Stop; break } catch { $attempt++; if ($attempt -ge 10) { throw }; Start-Sleep -Seconds 5 } }",
    )
    .run()
    .await?;
    pwsh("Start-VM -Name TestL2").run().await?;
    let state = pwsh("(Get-VM -Name TestL2).State").read().await?;
    if !state.contains("Running") {
        // Diagnostic-only: the nested L2 aborted (works on Intel+KVM, fails on
        // AMD+WHP). The L0 (openvmm) log shows no intercepts during nested
        // execution, so the only evidence for why the L2 stopped lives in the
        // L1 guest's Hyper-V event logs and VM status. Capture them before
        // failing. This does not change the L2's configuration.
        let l2_diag = pwsh(r#"
                Write-Host "=== Get-VM TestL2 (full) ==="
                Get-VM -Name TestL2 -ErrorAction SilentlyContinue |
                    Format-List Name,State,Status,PrimaryStatusDescription,SecondaryStatusDescription,CPUUsage,MemoryAssigned,Uptime,Version,Generation |
                    Out-String -Width 4096

                Write-Host "=== Get-VM TestL2 | Select * ==="
                Get-VM -Name TestL2 -ErrorAction SilentlyContinue | Select-Object * | Format-List | Out-String -Width 4096

                foreach ($log in @(
                    'Microsoft-Windows-Hyper-V-Worker-Admin',
                    'Microsoft-Windows-Hyper-V-Hypervisor-Admin',
                    'Microsoft-Windows-Hyper-V-VMMS-Admin',
                    'Microsoft-Windows-Hyper-V-Compute-Admin'
                )) {
                    Write-Host "=== $log (last 30) ==="
                    Get-WinEvent -LogName $log -MaxEvents 30 -ErrorAction SilentlyContinue |
                        Format-List TimeCreated,Id,LevelDisplayName,Message |
                        Out-String -Width 4096
                }

                Write-Host "=== System log: Hyper-V errors/warnings (last 100) ==="
                Get-WinEvent -LogName System -MaxEvents 100 -ErrorAction SilentlyContinue |
                    Where-Object {
                        ($_.LevelDisplayName -eq 'Error' -or $_.LevelDisplayName -eq 'Warning') -and
                        ($_.ProviderName -match 'Hyper-V|vid|vmms|vmcompute|hypervisor')
                    } |
                    Format-List TimeCreated,Id,ProviderName,LevelDisplayName,Message |
                    Out-String -Width 4096
            "#)
            .ignore_status()
            .read()
            .await?;
        tracing::error!("TestL2 did not reach Running (state={state}); L1 diagnostics:\n{l2_diag}");
        anyhow::bail!("L2 VM is not running: {state}");
    }
    pwsh("Stop-VM -Name TestL2 -TurnOff -Force; Remove-VM -Name TestL2 -Force")
        .run()
        .await?;

    // --- DDA (Discrete Device Assignment) test ---
    //
    // Find the extra NVMe controller on root port s0rc0rp3, dismount it
    // from the L1 host, assign it to a new L2 VM, and verify the L2 starts.

    // The OpenVMM NVMe emulator has PCI vendor 1414 (Microsoft), device
    // c03e. We find all matching controllers, pick the one on the
    // highest-numbered root port (our DDA target), and capture both its
    // PnP instance ID (for Disable-PnpDevice) and PCI location path
    // (for the DDA cmdlets).
    let dda_info = pwsh(r#"
            $devs = Get-PnpDevice -InstanceId 'PCI\VEN_1414&DEV_C03E*' -ErrorAction SilentlyContinue |
                Where-Object { $_.Status -eq 'OK' }
            if (-not $devs -or @($devs).Count -lt 2) {
                # Diagnostic: show all PCI devices
                Get-PnpDevice -ErrorAction SilentlyContinue |
                    Where-Object { $_.InstanceId -match '^PCI\\' } |
                    Format-List InstanceId,Class,FriendlyName,Status | Out-Host
                Write-Error "Expected at least 2 NVMe controllers (VEN_1414&DEV_C03E), found $(@($devs).Count)"
                exit 1
            }
            # Pick the controller on the highest-numbered root port.
            $best = $null
            $bestPath = $null
            foreach ($d in @($devs)) {
                $paths = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName DEVPKEY_Device_LocationPaths -ErrorAction SilentlyContinue).Data
                foreach ($p in $paths) {
                    if ($p -match 'PCIROOT' -and ($bestPath -eq $null -or $p -gt $bestPath)) {
                        $best = $d
                        $bestPath = $p
                    }
                }
            }
            if (-not $best) {
                Write-Error "Could not find PCI location path for any NVMe controller"
                exit 1
            }
            # Output instance ID on line 1, location path on line 2.
            Write-Output $best.InstanceId
            Write-Output $bestPath
        "#)
        .read()
        .await?;
    let mut lines = dda_info.lines().filter(|l| !l.trim().is_empty());
    let dda_instance_id = lines
        .next()
        .context("missing instance ID in DDA discovery output")?
        .trim();
    let dda_location_path = lines
        .next()
        .context("missing location path in DDA discovery output")?
        .trim();
    tracing::info!("DDA target: instance={dda_instance_id} location={dda_location_path}");

    // Disable the device before dismounting.
    pwsh(&format!(
        "Disable-PnpDevice -InstanceId '{}' -Confirm:$false",
        dda_instance_id
    ))
    .run()
    .await?;

    // Dismount the device from the host so it can be assigned to a VM.
    let dismount = pwsh(&format!(
        "Dismount-VMHostAssignableDevice -LocationPath '{}' -Force",
        dda_location_path
    ))
    .ignore_status()
    .output()
    .await?;

    if !dismount.status.success() {
        tracing::error!(
            stderr = %String::from_utf8_lossy(&dismount.stderr),
            "Dismount-VMHostAssignableDevice failed; capturing pcip PnP diagnostics"
        );

        // VMMS raises MSVM_VMMS_PROXY_FAILED_LOAD when, after the dismount
        // IOCTL succeeds and the devnode is re-enumerated, pcip.sys fails to
        // finish PnP-starting and publish its DDA device interface within the
        // install timeout. The dismount itself succeeded, so this is a pcip
        // PrepareHardware failure, not a reset-capability rejection. Capture
        // the re-enumerated PCIP devnode's problem code and the pcip / PnP
        // event logs to pinpoint which PrepareHardware step failed.
        let pnp_diag = pwsh(r#"
                Write-Host "=== pcip PnP devnode state ==="
                Get-PnpDevice -ErrorAction SilentlyContinue |
                    Where-Object { $_.InstanceId -match '^PCIP\\' -or $_.InstanceId -match 'VEN_1414&DEV_C03E' } |
                    ForEach-Object {
                        Write-Host "InstanceId: $($_.InstanceId)"
                        Write-Host "  Status: $($_.Status)  Class: $($_.Class)  FriendlyName: $($_.FriendlyName)"
                        $problem = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode' -ErrorAction SilentlyContinue).Data
                        $problemStatus = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_ProblemStatus' -ErrorAction SilentlyContinue).Data
                        Write-Host "  ProblemCode: $problem  ProblemStatus: $problemStatus"
                        $svc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
                        Write-Host "  Service: $svc"
                    }

                Write-Host "=== System log: pcip / PnP / VMMS errors (last 100) ==="
                Get-WinEvent -LogName System -MaxEvents 100 -ErrorAction SilentlyContinue |
                    Where-Object {
                        $_.ProviderName -match 'pcip|Pnp|Kernel-PnP|vpci' -or
                        $_.Message -match 'pcip|PCIP|proxy|assignable'
                    } |
                    Format-List TimeCreated,Id,ProviderName,LevelDisplayName,Message |
                    Out-String -Width 4096

                Write-Host "=== Kernel-PnP Admin/Config logs ==="
                foreach ($log in @(
                    'Microsoft-Windows-Kernel-PnP/Configuration',
                    'Microsoft-Windows-Hyper-V-VMMS-Admin'
                )) {
                    Write-Host "--- $log ---"
                    Get-WinEvent -LogName $log -MaxEvents 30 -ErrorAction SilentlyContinue |
                        Format-List TimeCreated,Id,LevelDisplayName,Message |
                        Out-String -Width 4096
                }
            "#)
            .ignore_status()
            .read()
            .await?;
        tracing::info!("pcip PnP diagnostics:\n{pnp_diag}");

        anyhow::bail!(
            "Dismount-VMHostAssignableDevice failed: {}",
            String::from_utf8_lossy(&dismount.stderr)
        );
    }

    // Create a Gen2 VM and assign the device to it.
    pwsh(
        "$attempt = 0; while ($attempt -lt 10) { try { New-VM -Name TestL2DDA -MemoryStartupBytes 512MB -Generation 2 -NoVHD -ErrorAction Stop; break } catch { $attempt++; if ($attempt -ge 10) { throw }; Start-Sleep -Seconds 5 } }",
    )
    .run()
    .await?;

    // Set the automatic stop action to TurnOff to avoid save-state issues
    // with assigned devices.
    pwsh("Set-VM -Name TestL2DDA -AutomaticStopAction TurnOff")
        .run()
        .await?;

    // Assign the device to the VM.
    pwsh(&format!(
        "Add-VMAssignableDevice -VMName TestL2DDA -LocationPath '{}'",
        dda_location_path
    ))
    .run()
    .await?;

    // Verify the device is listed as assigned.
    let assigned_devs = pwsh("Get-VMAssignableDevice -VMName TestL2DDA | Format-List")
        .read()
        .await?;
    tracing::info!("Assigned devices on TestL2DDA:\n{assigned_devs}");
    assert!(
        !assigned_devs.trim().is_empty(),
        "No devices assigned to TestL2DDA"
    );

    // Relax both DDA power-on policies before starting the VM. Hyper-V gates
    // DDA power-on behind two policies (the failure cites both):
    //   * RequireSupportedDeviceAssignment — an allowlist / capability gate
    //     (the device model being "blessed", etc.).
    //   * RequireSecureDeviceAssignment — the secure-assignment gate, which
    //     among other things requires the device to expose a reset mechanism
    //     (Function-Level Reset) so its state can be scrubbed when it changes
    //     owners.
    // OpenVMM's emulated NVMe controller does not yet advertise FLR (it is
    // constructed with no FlrHandler, so the PCIe Device Capabilities FLR bit
    // reads 0), which fails the secure gate — clearing only the supported
    // policy is not enough. Until the NVMe device implements FLR, clear both
    // policies so the assignment can proceed. This is a temporary workaround;
    // the real fix is to wire up an FlrHandler on the NVMe device.
    pwsh(
        r#"New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\HyperV' -Force | Out-Null; New-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\HyperV' -Name 'RequireSupportedDeviceAssignment' -PropertyType DWord -Value 0 -Force | Out-Null; New-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\HyperV' -Name 'RequireSecureDeviceAssignment' -PropertyType DWord -Value 0 -Force | Out-Null"#,
    )
    .run()
    .await?;

    // Start the L2 VM with the assigned device.
    pwsh("Start-VM -Name TestL2DDA").run().await?;
    let state = pwsh("(Get-VM -Name TestL2DDA).State").read().await?;
    assert!(
        state.contains("Running"),
        "L2 DDA VM is not running: {state}"
    );

    // Clean up.
    pwsh("Stop-VM -Name TestL2DDA -TurnOff -Force; Remove-VM -Name TestL2DDA -Force")
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
