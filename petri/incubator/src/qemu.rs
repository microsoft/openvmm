// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU device provisioning.

use crate::profile::DeviceConfig;
use crate::profile::parse_size;
use anyhow::Context;
use std::collections::BTreeMap;

/// First PCI device number (`addr=`) used for extra-device root ports.
///
/// QEMU's built-in devices use low device numbers. We start at 16 (0x10)
/// to avoid collisions. The root port for the i-th extra device has
/// devfn = `(EXTRA_DEVICE_ADDR_BASE + i) << 3`.
const EXTRA_DEVICE_ADDR_BASE: usize = petri::qemu::EXTRA_DEVICE_ADDR_BASE;

pub(crate) fn petri_devices(
    devices: &[DeviceConfig],
) -> anyhow::Result<Vec<petri::qemu::devices::DeviceConfig>> {
    use petri::qemu::devices as p;
    devices
        .iter()
        .map(|device| {
            Ok(match device {
                DeviceConfig::VirtioBlk(c) => {
                    p::DeviceConfig::VirtioBlk(p::VirtioBlkDeviceConfig {
                        name: c.name.clone(),
                        size: parse_size(&c.size)?,
                        vfio: c.vfio,
                    })
                }
                DeviceConfig::Edu(c) => p::DeviceConfig::Edu(p::EduDeviceConfig {
                    name: c.name.clone(),
                    dma_mask: c
                        .dma_mask
                        .as_deref()
                        .map(|value| {
                            if let Some(hex) = value.strip_prefix("0x") {
                                u64::from_str_radix(hex, 16)
                            } else {
                                value.parse()
                            }
                            .context("invalid DMA mask")
                        })
                        .transpose()?,
                    vfio: c.vfio,
                }),
                DeviceConfig::IvshmemPlain(c) => {
                    p::DeviceConfig::IvshmemPlain(p::IvshmemPlainDeviceConfig {
                        name: c.name.clone(),
                        size: parse_size(&c.size)?,
                        vfio: c.vfio,
                    })
                }
            })
        })
        .collect()
}

/// Set up VFIO devices inside the incubator.
///
/// Each extra device in the profile sits behind its own PCIe root port
/// at a known PCI device number (see [`EXTRA_DEVICE_ADDR_BASE`]). This
/// function discovers the child device's BDF by finding the bridge at
/// that slot in sysfs, then unbinds the child from its driver and binds
/// it to vfio-pci.
///
/// Returns a map of environment variables to set for the guest command,
/// e.g., `INCUBATOR_VFIO_BDF_TEST_DISK=0000:01:00.0`. If any provisioned
/// device declares a `provides` capability, the returned map also includes
/// `PETRI_CAPABILITIES` listing those capabilities (comma-separated).
pub async fn setup_vfio_devices(
    client: &pipette_client::PipetteClient,
    devices: &[DeviceConfig],
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    let mut capabilities = Vec::new();

    // Collect (device_index, device) for devices that need VFIO binding.
    // VFIO binding is device-type-agnostic: any extra device behind its own
    // root port can be unbound from its driver and rebound to vfio-pci.
    let vfio_devices: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.vfio())
        .collect();

    if vfio_devices.is_empty() {
        return Ok(env);
    }

    tracing::info!("setting up {} VFIO device(s)", vfio_devices.len());

    for (device_index, device) in &vfio_devices {
        let name = device.name();
        let addr = EXTRA_DEVICE_ADDR_BASE + device_index;

        // The root port for this device is deterministically at
        // 0000:00:{addr:02x}.0 (see `build_qemu_command`). Read its
        // secondary bus number from sysfs; the assigned device sits at
        // slot 0, function 0 of that bus.
        let rp_bdf = format!("0000:00:{addr:02x}.0");
        let secondary_bus_path = format!("/sys/bus/pci/devices/{rp_bdf}/secondary_bus_number");
        let secondary_bus_raw = client
            .read_file(&secondary_bus_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read secondary bus number for device '{name}' (root port {rp_bdf})"
                )
            })?;
        // sysfs reports the secondary bus number in decimal.
        let secondary_bus_str = String::from_utf8_lossy(&secondary_bus_raw);
        let secondary_bus: u8 = secondary_bus_str.trim().parse().with_context(|| {
            format!("unexpected secondary bus number {secondary_bus_str:?} for device '{name}'")
        })?;
        let bdf = format!("0000:{secondary_bus:02x}:00.0");

        // Confirm the child device actually exists before trying to rebind it.
        client
            .read_file(format!("/sys/bus/pci/devices/{bdf}/vendor"))
            .await
            .with_context(|| {
                format!(
                    "no device found behind root port {rp_bdf} (expected {bdf}) for device '{name}'"
                )
            })?;

        tracing::info!(%name, %bdf, %addr, "binding device to vfio-pci");

        // Unbind from current driver
        let _ = client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver/unbind"),
                bdf.as_bytes(),
            )
            .await;

        // Set driver override to vfio-pci
        client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver_override"),
                b"vfio-pci".as_slice(),
            )
            .await
            .context("failed to set driver_override")?;

        // Bind to vfio-pci
        client
            .write_file("/sys/bus/pci/drivers/vfio-pci/bind", bdf.as_bytes())
            .await
            .context("failed to bind to vfio-pci")?;

        // Export env var: name "test-disk" → INCUBATOR_VFIO_BDF_TEST_DISK
        let env_name = format!(
            "INCUBATOR_VFIO_BDF_{}",
            name.to_uppercase().replace('-', "_")
        );
        tracing::info!(%env_name, %bdf, "VFIO device ready");
        env.insert(env_name, bdf);

        // Advertise the capability this device provides (derived from its
        // name), now that it has been successfully provisioned. Tests gate on
        // this via `requires(...)`.
        capabilities.push(device.capability());
    }

    // Advertise all provisioned capabilities to the guest command via
    // PETRI_CAPABILITIES (comma-separated), which petri's requirement
    // evaluation reads. Augment any capabilities already present in the
    // incubator's environment rather than overwriting them, so that
    // host-provided capabilities are preserved.
    if !capabilities.is_empty() {
        let mut value = capabilities.join(",");
        if let Ok(existing) = std::env::var("PETRI_CAPABILITIES") {
            if !existing.is_empty() {
                value = format!("{existing},{value}");
            }
        }
        env.insert("PETRI_CAPABILITIES".to_string(), value);
    }

    Ok(env)
}
