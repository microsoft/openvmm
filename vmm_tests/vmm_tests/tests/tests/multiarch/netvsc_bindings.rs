// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows netvsc adapter binding-stack tests.

use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use pipette_client::shell::WindowsShell;
use std::time::Duration;
use vmm_test_macros::vmm_test_with;

async fn wait_for_network(
    sh: &WindowsShell<'_>,
    driver: &DefaultDriver,
    mac_address: &str,
) -> anyhow::Result<()> {
    let script = format!(
        r#"
            $adapter = @(Get-NetAdapter | Where-Object {{ $_.MacAddress -eq '{mac_address}' }})
            if ($adapter.Count -ne 1) {{
                Write-Output "expected one adapter, found $($adapter.Count)"
                exit 1
            }}
            if ($adapter[0].Status -ne 'Up') {{
                Write-Output "adapter status is $($adapter[0].Status)"
                exit 1
            }}
            $address = Get-NetIPAddress -InterfaceIndex $adapter[0].InterfaceIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
                Where-Object {{ $_.IPAddress -eq '10.0.0.2' }}
            if (-not $address) {{
                Write-Output "adapter does not have 10.0.0.2"
                exit 1
            }}
            if (-not (Test-Connection -ComputerName 10.0.0.1 -Count 1 -Quiet)) {{
                Write-Output "gateway ping failed"
                exit 1
            }}
            Write-Output "READY"
        "#
    );

    let mut timer = PolledTimer::new(driver);
    let mut last_output = String::new();
    for attempt in 0..30 {
        last_output = cmd!(sh, "powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .ignore_status()
            .read()
            .await?;
        if last_output.lines().any(|line| line.trim() == "READY") {
            tracing::info!(attempt, "synthetic NIC has working connectivity");
            return Ok(());
        }
        tracing::debug!(attempt, %last_output, "waiting for synthetic NIC connectivity");
        timer.sleep(Duration::from_secs(2)).await;
    }

    anyhow::bail!("synthetic NIC did not recover connectivity: {last_output}")
}

async fn verify_ms_pacer_binding(
    sh: &WindowsShell<'_>,
    mac_address: &str,
    expected_enabled: bool,
) -> anyhow::Result<()> {
    let expected = if expected_enabled { "$true" } else { "$false" };
    let script = format!(
        r#"
            $adapter = @(Get-NetAdapter | Where-Object {{ $_.MacAddress -eq '{mac_address}' }})
            if ($adapter.Count -ne 1) {{
                throw "Expected one adapter with MAC {mac_address}, found $($adapter.Count)"
            }}
            $binding = Get-NetAdapterBinding -Name $adapter[0].Name -ComponentID ms_pacer -ErrorAction Stop
            if ($binding.Enabled -ne {expected}) {{
                throw "Expected ms_pacer Enabled={expected_enabled}, got $($binding.Enabled)"
            }}
        "#
    );
    cmd!(sh, "powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .run()
        .await?;
    Ok(())
}

#[vmm_test_with(
    openvmm,
    requires(windows_partition_reset),
    configs(uefi_x64(vhd(windows_datacenter_core_2022_x64)))
)]
async fn windows_netvsc_ms_pacer_unbind(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config.modify_backend(|b| b.with_nic()).run().await?;
    let sh = agent.windows_shell();
    let mac_address = NIC_MAC_ADDRESS.to_string();

    wait_for_network(&sh, &driver, &mac_address).await?;

    let disable_script = format!(
        r#"
            $adapter = @(Get-NetAdapter | Where-Object {{ $_.MacAddress -eq '{mac_address}' }})
            if ($adapter.Count -ne 1) {{
                throw "Expected one adapter with MAC {mac_address}, found $($adapter.Count)"
            }}
            $binding = Get-NetAdapterBinding -Name $adapter[0].Name -ComponentID ms_pacer -ErrorAction Stop
            if (-not $binding.Enabled) {{
                throw "ms_pacer is not enabled on $($adapter[0].Name)"
            }}
            Disable-NetAdapterBinding -Name $adapter[0].Name -ComponentID ms_pacer -Confirm:$false -ErrorAction Stop
            $binding = Get-NetAdapterBinding -Name $adapter[0].Name -ComponentID ms_pacer -ErrorAction Stop
            if ($binding.Enabled) {{
                throw "ms_pacer is still enabled on $($adapter[0].Name)"
            }}
        "#
    );
    cmd!(sh, "powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &disable_script])
        .run()
        .await?;

    wait_for_network(&sh, &driver, &mac_address).await?;

    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    let sh = agent.windows_shell();

    verify_ms_pacer_binding(&sh, &mac_address, false).await?;
    wait_for_network(&sh, &driver, &mac_address).await?;

    let enable_script = format!(
        r#"
            $adapter = @(Get-NetAdapter | Where-Object {{ $_.MacAddress -eq '{mac_address}' }})
            if ($adapter.Count -ne 1) {{
                throw "Expected one adapter with MAC {mac_address}, found $($adapter.Count)"
            }}
            Enable-NetAdapterBinding -Name $adapter[0].Name -ComponentID ms_pacer -Confirm:$false -ErrorAction Stop
        "#
    );
    cmd!(sh, "powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &enable_script])
        .run()
        .await?;

    verify_ms_pacer_binding(&sh, &mac_address, true).await?;
    wait_for_network(&sh, &driver, &mac_address).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
