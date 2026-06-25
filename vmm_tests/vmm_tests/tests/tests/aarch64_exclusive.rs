// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for aarch64 guests.

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use vm_resource::IntoResource;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_with;

/// Boot Linux and verify the PMU interrupt is available.
///
/// TODO: This is only supported on WHP and Hyper-V.
///
#[vmm_test(
    // TODO: requires aarch64 serial emulator changes, or petri changes to use
    // something other than serial. GH issue 1790.
    //
    // openvmm_linux_direct_aarch64,
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pmu_gsiv<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.run().await?;

    // Check dmesg for logs about the PMU.
    let shell = agent.unix_shell();
    let dmesg = cmd!(shell, "dmesg").read().await?;

    // There should be no lines that look like the following:
    //  "No ACPI PMU IRQ for CPU0"
    dmesg.lines().try_for_each(|line| {
        if line.contains("No ACPI PMU IRQ for CPU") {
            Err(anyhow::anyhow!("PMU IRQ not found in dmesg: {}", line))
        } else {
            Ok(())
        }
    })?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot ARM64 Linux in device-tree mode (full DT, no ACPI).
// TODO: disabled until we get a kernel that supports DT boot with the
// current device configuration.
// #[openvmm_test(linux_direct_aarch64)]
#[expect(dead_code)]
async fn boot_dt(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .modify_backend(|c| {
            c.with_custom_config(|c| {
                if let openvmm_defs::config::LoadMode::Linux { boot_mode, .. } = &mut c.load_mode {
                    *boot_mode = openvmm_defs::config::LinuxDirectBootMode::DeviceTree;
                }
            })
        })
        .run()
        .await?;

    // Verify we're in DT mode — no ACPI tables directory.
    let shell = agent.unix_shell();
    let output = cmd!(shell, "test -d /sys/firmware/acpi/tables")
        .ignore_status()
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "ACPI tables should not exist in DT-only mode"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot an aarch64 guest with no VMBus via linux direct boot,
/// and assign a VFIO device from the incubator into the guest.
///
/// This test is intended to run inside a QEMU TCG incubator with KVM.
/// The incubator profile sets up a virtio-blk device bound to vfio-pci,
/// and exports its BDF via the `INCUBATOR_VFIO_BDF_TEST_DISK` env var.
/// The test assigns that device into the L2 guest and verifies it appears
/// as a block device, then reads from it to exercise DMA and interrupts.
///
/// The `_aarch64_tcg` name suffix opts this test into the TCG incubator
/// pass: CI selects it via the `test(aarch64_tcg)` nextest filter.
#[vmm_test_with(
    (
        openvmm,
        requires(test_disk_vfio)
    ),
    (
        linux_direct_aarch64
    )
)]
async fn boot_no_vmbus_pcie_aarch64_tcg(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // Read the VFIO BDF from the environment. This is set by the incubator
    // when it binds a device to vfio-pci before running the test.
    // The "test_disk_vfio" capability requirement ensures this is set before
    // the test runs.
    let vfio_bdf = std::env::var("INCUBATOR_VFIO_BDF_TEST_DISK").unwrap();

    tracing::info!(vfio_bdf = %vfio_bdf, "assigning VFIO device to guest");

    // Open the VFIO cdev and iommufd for this device.
    let sysfs_path = std::path::Path::new("/sys/bus/pci/devices").join(&vfio_bdf);
    let vfio_dev_dir = sysfs_path.join("vfio-dev");
    let cdev_name = std::fs::read_dir(&vfio_dev_dir)
        .with_context(|| {
            format!(
                "failed to read {}: is {} bound to vfio-pci?",
                vfio_dev_dir.display(),
                vfio_bdf
            )
        })?
        .next()
        .context("no vfio-dev entry found")?
        .context("failed to read vfio-dev entry")?;
    let cdev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::path::Path::new("/dev/vfio/devices").join(cdev_name.file_name()))
        .context("failed to open VFIO cdev")?;
    let iommufd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/iommu")
        .context("failed to open /dev/iommu")?;

    let (vm, agent) = config
        .with_no_vmbus()
        .with_memory(petri::MemoryConfig {
            startup_bytes: 1024 * 1024 * 1024,
            ..Default::default()
        })
        .modify_backend(move |b| {
            b.with_pcie_root_topology(1, 1, 3).with_custom_config(|c| {
                c.hypervisor.with_hv = false;
                c.pcie_devices.push(openvmm_defs::config::PcieDeviceConfig {
                    port_name: "s0rc0rp1".into(),
                    resource: vfio_assigned_device_resources::VfioCdevDeviceHandle {
                        pci_id: vfio_bdf,
                        cdev,
                        iommufd,
                        iommu_id: "iommu0".into(),
                        bar_pt: [false; 6],
                    }
                    .into_resource(),
                });
            })
        })
        .run()
        .await?;

    // Verify the assigned device appears in the guest as /dev/vda with the
    // expected size. The incubator provisions a 64 MiB VFIO-backed virtio-blk
    // disk (the `test-disk` device in the aarch64-tcg-pcie incubator profile).
    // Checking the sysfs size proves the VFIO-assigned device is the one that
    // showed up, rather than merely that *some* vda exists.
    const TEST_DISK_SIZE: u64 = 64 * 1024 * 1024;
    let sh = agent.unix_shell();
    let vda_size = sh
        .read_file("/sys/block/vda/size")
        .await
        .context("VFIO-assigned virtio-blk device /dev/vda not found")?;
    let vda_sectors: u64 = vda_size.trim().parse().context("parse vda size")?;
    tracing::info!(vda_sectors, "guest /dev/vda size");
    anyhow::ensure!(
        vda_sectors == TEST_DISK_SIZE / 512,
        "unexpected /dev/vda size: expected {} sectors, got {vda_sectors}",
        TEST_DISK_SIZE / 512
    );

    // Read from the disk to exercise DMA and interrupts through the IOMMU.
    let dd_output = cmd!(sh, "dd if=/dev/vda of=/dev/null bs=4096 count=16")
        .read_stderr()
        .await?;
    tracing::info!(dd_output = %dd_output, "dd completed");
    anyhow::ensure!(
        dd_output.contains("16+0 records"),
        "expected 16 records read, got: {dd_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
