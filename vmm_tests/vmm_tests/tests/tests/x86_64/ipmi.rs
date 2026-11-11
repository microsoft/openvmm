// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the OpenHCL IPMI KCS interface.

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use vmm_test_macros::openvmm_test;

const LINUX_IPMI_TEST: &str = include_str!("../../../test_data/ipmi_add_sel.py");

const WINDOWS_IPMI_TEST: &str = r#"
$ipmi = Get-CimInstance -Namespace root\wmi -ClassName Microsoft_IPMI -ErrorAction Stop
$record = [byte[]](
    0x00,0x00,0x02,0x00,0x00,0x00,0x00,0x20,
    0x00,0x04,0x09,0x01,0x6f,0xde,0xad,0xbe
)
$response = Invoke-CimMethod -InputObject $ipmi -MethodName RequestResponse -Arguments @{
    NetworkFunction  = [byte]0x0A
    Lun              = [byte]0x00
    ResponderAddress = [byte]0x20
    Command          = [byte]0x44
    RequestData      = $record
    RequestDataSize  = [uint32]$record.Length
} -ErrorAction Stop
if ($response.CompletionCode -ne 0) {
    throw "Add SEL failed with completion code $($response.CompletionCode)"
}
Write-Output "ADDSEL_CC=0"
"#;

#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn ipmi_kcs_add_sel(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (mut vm, agent) = config.with_ipmi(true).run().await?;

    let output = match os_flavor {
        OsFlavor::Linux => {
            let shell = agent.unix_shell();
            cmd!(shell, "sudo modprobe ipmi_si").run().await?;
            cmd!(shell, "sudo modprobe ipmi_devintf").run().await?;
            agent
                .write_file("/tmp/ipmi_add_sel.py", LINUX_IPMI_TEST.as_bytes())
                .await
                .context("failed to copy the Linux IPMI test into the guest")?;
            cmd!(shell, "sudo python3 /tmp/ipmi_add_sel.py")
                .read()
                .await?
        }
        OsFlavor::Windows => {
            let shell = agent.windows_shell();
            cmd!(shell, "powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    WINDOWS_IPMI_TEST,
                ])
                .read()
                .await?
        }
        _ => unreachable!(),
    };

    anyhow::ensure!(
        output.contains("ADDSEL_CC=0"),
        "guest did not successfully add an IPMI SEL record: {output}"
    );

    let notification = loop {
        let notification = vm.backend().wait_for_ipmi_sel().await?;
        if notification.record[2] == 0x02
            && notification.record[7..] == [0x20, 0x00, 0x04, 0x09, 0x01, 0x6f, 0xde, 0xad, 0xbe]
        {
            break notification;
        }

        tracing::info!(?notification, "ignoring an unrelated IPMI SEL notification");
    };
    anyhow::ensure!(
        notification.record_id != 0
            && notification.record[0..2] == notification.record_id.to_le_bytes(),
        "host received an invalid IPMI SEL record ID: {notification:?}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
