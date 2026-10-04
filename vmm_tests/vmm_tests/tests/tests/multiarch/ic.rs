// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use hyperv_ic_resources::kvp::KvpRpc;
use jiff::SignedDuration;
use mesh::rpc::RpcSend;
use petri::PetriLogSource;
use petri::PetriVmBuilder;
use petri::openvmm::NIC_MAC_ADDRESS;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use std::time::Duration;
use vmm_test_macros::openvmm_test;

async fn capture_wmi_snapshot(agent: &PipetteClient, logs: &PetriLogSource) {
    let shell = agent.windows_shell();
    match cmd!(shell, "D:/pipette.exe")
        .arg("--diagnose-wmi")
        .ignore_status()
        .output()
        .await
    {
        Ok(output) => {
            if let Err(error) =
                logs.write_attachment("kvp-wmi-snapshot.log", output.stdout.as_slice())
            {
                tracing::warn!(%error, "could not attach guest WMI snapshot");
            }
            if !output.status.success() {
                tracing::warn!(
                    status = %output.status,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "guest WMI snapshot failed"
                );
            }
        }
        Err(error) => tracing::warn!(%error, "could not capture guest WMI snapshot"),
    }
}

async fn capture_wmi_events(agent: &PipetteClient, logs: &PetriLogSource) {
    let shell = agent.windows_shell();
    for (channel, query, attachment) in [
        (
            "Microsoft-Windows-WMI-Activity/Operational",
            "*[System[TimeCreated[timediff(@SystemTime)<=300000]]]",
            "kvp-wmi-events.log",
        ),
        (
            "System",
            "*[System[Provider[@Name='Microsoft-Windows-DistributedCOM'] and TimeCreated[timediff(@SystemTime)<=300000]]]",
            "kvp-dcom-events.log",
        ),
    ] {
        match cmd!(shell, "wevtutil.exe")
            .args(["qe", channel, "/rd:true", "/c:40", "/f:text"])
            .arg(format!("/q:{query}"))
            .ignore_status()
            .output()
            .await
        {
            Ok(output) => {
                if let Err(error) = logs.write_attachment(attachment, output.stdout.as_slice()) {
                    tracing::warn!(%error, channel, "could not attach guest event log");
                }
                if !output.status.success() {
                    tracing::warn!(
                        status = %output.status,
                        stderr = %String::from_utf8_lossy(&output.stderr),
                        channel,
                        "could not query guest event log"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, channel, "could not query guest event log"),
        }
    }
}

/// Test the KVP IC.
///
/// Windows-only right now, because the Linux images do not include the KVP IC
/// daemon.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64)))]
async fn kvp_ic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    // Run with a NIC to perform IP address tests.
    let logs = config.log_source().clone();
    let (mut vm, agent) = config.modify_backend(|c| c.with_nic()).run().await?;
    let kvp = vm.backend().wait_for_kvp().await?;

    // Perform a basic set and enumerate test.
    let test_key = "test_key";
    let test_value = hyperv_ic_resources::kvp::Value::String("test_value".to_string());
    kvp.call_failable(
        KvpRpc::Set,
        hyperv_ic_resources::kvp::SetParams {
            pool: hyperv_ic_resources::kvp::KvpPool::External,
            key: test_key.to_string(),
            value: test_value.clone(),
        },
    )
    .await?;
    let value = kvp
        .call_failable(
            KvpRpc::Enumerate,
            hyperv_ic_resources::kvp::EnumerateParams {
                pool: hyperv_ic_resources::kvp::KvpPool::External,
                index: 0,
            },
        )
        .await?
        .context("missing value")?;
    assert_eq!(value.key, test_key);
    assert_eq!(value.value, test_value.clone());

    let value = kvp
        .call_failable(
            KvpRpc::Enumerate,
            hyperv_ic_resources::kvp::EnumerateParams {
                pool: hyperv_ic_resources::kvp::KvpPool::External,
                index: 1,
            },
        )
        .await?;

    assert!(value.is_none());

    // Get IP information for the NIC.
    let ip_info_request = kvp.call_failable(
        KvpRpc::GetIpInfo,
        hyperv_ic_resources::kvp::GetIpInfoParams {
            adapter_id: NIC_MAC_ADDRESS.to_string().replace('-', ":"),
        },
    );
    let mut ip_info_request = std::pin::pin!(ip_info_request);
    let ip_info = match mesh::CancelContext::new()
        .with_timeout(Duration::from_secs(20))
        .until_cancelled(&mut ip_info_request)
        .await
    {
        Ok(result) => result,
        Err(reason) => {
            tracing::warn!(
                ?reason,
                "KVP GetIpInfo is still pending; capturing guest WMI state"
            );
            let (result, ()) = futures::join!(ip_info_request, capture_wmi_snapshot(&agent, &logs));
            result
        }
    };
    if ip_info.is_err() {
        capture_wmi_events(&agent, &logs).await;
    }
    let ip_info = ip_info?;

    // Validate the IP information against the default consomme configuration.
    tracing::info!(?ip_info, "ip information");

    // Filter out link-local addresses, since Windows seems to enumerate one for
    // a little while after boot sometimes.
    let non_local_ipv4_addresses = ip_info
        .ipv4_addresses
        .iter()
        .filter(|ip| !ip.address.is_link_local())
        .collect::<Vec<_>>();

    assert_eq!(non_local_ipv4_addresses.len(), 1);
    let ip = &non_local_ipv4_addresses[0];
    assert_eq!(ip.address.to_string(), "10.0.0.2");
    assert_eq!(ip.subnet.to_string(), "255.255.255.0");
    assert_eq!(ip_info.ipv4_gateways.len(), 1);
    let gateway = &ip_info.ipv4_gateways[0];
    assert_eq!(gateway.to_string(), "10.0.0.1");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test the timesync IC.
#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2504_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    linux_direct_x64
)]
async fn timesync_ic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                // Start with the clock half a day in the past so that the clock is
                // initially wrong.
                c.rtc_delta_milliseconds = -(Duration::from_secs(40000).as_millis() as i64)
            })
        })
        .run()
        .await?;

    let mut saw_time_sync = false;
    for _ in 0..30 {
        let time = agent.get_time().await?;
        let time = jiff::Timestamp::new(time.seconds, time.nanos).unwrap();
        tracing::info!(%time, "guest time");
        if time.duration_since(jiff::Timestamp::now()).abs() < SignedDuration::from_secs(10) {
            saw_time_sync = true;
            break;
        }
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .cancelled()
            .await;
    }

    if !saw_time_sync {
        anyhow::bail!("time never synchronized");
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
