// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utility functions for VMM tests that can be shared across multiple test
//! files.

use anyhow::Context;
use guid::Guid;
use pipette_client::PipetteClient;
use pipette_client::cmd;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(crate) struct ExpectedGuestDevice {
    pub lun: u32,
    pub disk_size_sectors: usize,
    #[expect(dead_code)] // Only used in logging via `Debug` trait
    pub friendly_name: String,
}

/// Get the device paths for the expected devices inside the Linux guest,
/// verifying that they exist and have the expected size.
pub(crate) async fn get_device_paths(
    agent: &PipetteClient,
    controller_guid: Guid,
    expected_devices: Vec<ExpectedGuestDevice>,
) -> anyhow::Result<Vec<String>> {
    let sh = agent.unix_shell();

    let all_disks = cmd!(sh, "sh -c 'ls -ld /sys/block/sd*'").read().await?;
    tracing::info!(?all_disks, "All disks");

    // Check that the correct devices are found in the VTL0 guest.
    // The test framework adds additional devices (pipette, cloud-init, etc), so
    // just check that the expected devices are indeed found.
    let mut device_paths = Vec::new();
    for d in &expected_devices {
        let list_sdx_cmd = format!(
            "ls -d /sys/bus/vmbus/devices/{}/host*/target*/*:0:0:{}/block/sd*",
            controller_guid, d.lun
        );
        let devices = cmd!(sh, "sh -c {list_sdx_cmd}").read().await?;
        let mut devices_iter = devices.lines();
        let dev = devices_iter.next().ok_or(anyhow::anyhow!(
            "Couldn't find device for controller {:#} lun {}",
            controller_guid,
            d.lun
        ))?;
        if devices_iter.next().is_some() {
            anyhow::bail!(
                "More than 1 device for controller {:#} lun {}",
                controller_guid,
                d.lun
            );
        }
        let dev = dev
            .rsplit('/')
            .next()
            .ok_or(anyhow::anyhow!("Couldn't parse device name from {dev}"))?;
        let sectors = cmd!(sh, "cat /sys/block/{dev}/size")
            .read()
            .await?
            .trim_end()
            .parse::<usize>()
            .context(format!(
                "Failed to parse size of device for controller {:#} lun {}",
                controller_guid, d.lun
            ))?;
        if sectors != d.disk_size_sectors {
            anyhow::bail!(
                "Unexpected size (in sectors) for device for controller {:#} lun {}: expected {}, got {}",
                controller_guid,
                d.lun,
                d.disk_size_sectors,
                sectors
            );
        }

        device_paths.push(format!("/dev/{dev}"));
    }

    // Check duplicates
    if device_paths.iter().collect::<HashSet<_>>().len() != device_paths.len() {
        anyhow::bail!("Found duplicate device paths: {device_paths:?}");
    }

    // Check that we found all devices and no extra devices are present
    let list_sdx_cmd = format!(
        // Don't fail if no devices are found
        "ls -d /sys/bus/vmbus/devices/{}/host*/target*/*:0:0:*/block/sd* || true",
        controller_guid
    );
    let devices = cmd!(sh, "sh -c {list_sdx_cmd}").read().await?;
    let devices_count = devices.lines().count();
    if devices_count != expected_devices.len() {
        anyhow::bail!(
            "Expected {} devices, found {} devices: {:?}",
            expected_devices.len(),
            devices_count,
            devices
        );
    }

    Ok(device_paths)
}
