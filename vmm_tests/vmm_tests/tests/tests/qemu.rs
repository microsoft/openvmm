// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test the QEMU emulator petri backend

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::qemu::EXTRA_DEVICE_ADDR_BASE;
use petri::qemu::QemuPetriBackend;
use petri::qemu::SHARE_9P_MOUNT_TAG;
use petri::qemu::devices::DeviceConfig;
use petri::qemu::devices::EduDeviceConfig;
use petri::qemu::devices::IvshmemPlainDeviceConfig;
use petri::qemu::devices::VirtioBlkDeviceConfig;
use pipette_client::cmd;
use vmm_test_macros::vmm_test_with;

/// This test is a step toward full native support testing openvmm using
/// the qemu aarch64 system emulator. It demonstrates that petri can setup
/// the same emulator that incubator creates in our existing tests.
#[vmm_test_with(qemu, configs(linux_direct_aarch64))]
async fn validate_emulator(config: PetriVmBuilder<QemuPetriBackend>) -> anyhow::Result<()> {
    const TEST_FILE_NAME_IN: &str = "test_in.txt";
    const TEST_FILE_DATA_IN: &str = "hello inside";
    const TEST_FILE_NAME_OUT: &str = "test_out.txt";
    const TEST_FILE_DATA_OUT: &str = "hello outside";
    const MEGABYTE: u64 = 1024 * 1024;

    let share = tempfile::tempdir()?;
    let share_path = share.path().to_path_buf();

    let devices = vec![
        DeviceConfig::VirtioBlk(VirtioBlkDeviceConfig {
            name: "test-disk".into(),
            size: 64 * MEGABYTE, // 64M
            vfio: true,
        }),
        DeviceConfig::Edu(EduDeviceConfig {
            name: "edu-initiator".into(),
            dma_mask: Some(0xffffffffffff),
            vfio: true,
        }),
        DeviceConfig::IvshmemPlain(IvshmemPlainDeviceConfig {
            name: "ivshmem-target".into(),
            size: 4 * MEGABYTE, // 4M
            vfio: true,
        }),
    ];

    let (vm, agent) = config
        .modify_backend({
            let share_path = share_path.clone();
            let devices = devices.clone();
            move |b| b.with_share_9p(share_path).with_devices(devices)
        })
        .run()
        .await?;

    validate_vfio_devices(&agent, &devices).await?;

    let guest_share_root = "/share";

    let sh = agent.unix_shell();
    cmd!(sh, "mkdir -p {guest_share_root}").run().await?;
    cmd!(
        sh,
        "mount -t 9p -o trans=virtio,version=9p2000.L {SHARE_9P_MOUNT_TAG} {guest_share_root}"
    )
    .run()
    .await?;

    fs_err::write(share_path.join(TEST_FILE_NAME_IN), TEST_FILE_DATA_IN)?;
    let in_file_data = agent
        .read_file(format!("{guest_share_root}/{TEST_FILE_NAME_IN}"))
        .await
        .context("failed to read file from the outside on the inside")?;
    if in_file_data != TEST_FILE_DATA_IN.as_bytes() {
        anyhow::bail!("data from outside does not match inside");
    }

    agent
        .write_file(
            format!("{guest_share_root}/{TEST_FILE_NAME_OUT}"),
            TEST_FILE_DATA_OUT.as_bytes(),
        )
        .await
        .context("failed to write file on the inside")?;
    let out_file_data = fs_err::read(share_path.join(TEST_FILE_NAME_OUT))?;
    if out_file_data != TEST_FILE_DATA_OUT.as_bytes() {
        anyhow::bail!("data from inside does not match outside");
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Set up VFIO devices.
///
/// Each extra device in the profile sits behind its own PCIe root port
/// at a known PCI device number (see [`EXTRA_DEVICE_ADDR_BASE`]). This
/// function discovers the child device's BDF by finding the bridge at
/// that slot in sysfs, then unbinds the child from its driver and binds
/// it to vfio-pci.
async fn validate_vfio_devices(
    client: &pipette_client::PipetteClient,
    devices: &[DeviceConfig],
) -> anyhow::Result<()> {
    for (device_index, device) in devices.iter().enumerate().filter(|(_, d)| d.vfio()) {
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

        tracing::info!(%bdf, "VFIO device ready");

        // Make sure the device exists
        let sh = client.unix_shell();
        let cdev = cmd!(sh, "ls /sys/bus/pci/devices/{bdf}/vfio-dev")
            .read()
            .await?;
        if cdev.trim().is_empty() {
            anyhow::bail!("no vfio-dev entry found");
        }
        cmd!(sh, "stat /dev/vfio/devices/{cdev}").run().await?;
    }

    Ok(())
}
