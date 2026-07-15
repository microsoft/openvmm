// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for aarch64 guests.

use anyhow::Context;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use std::time::Duration;
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
/// and publishes its BDF under the profile name `test-disk` (see
/// [`incubator_vfio_bdf`]). The test assigns that device into the L2 guest
/// and verifies it appears as a block device, then reads from it to exercise
/// DMA and interrupts.
///
/// The `_aarch64_tcg` name suffix opts this test into the TCG incubator
/// pass: CI selects it via the `test(aarch64_tcg)` nextest filter.
#[vmm_test_with(openvmm, requires(test_disk), configs(linux_direct_aarch64))]
async fn boot_no_vmbus_pcie_aarch64_tcg(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // Look up the assigned device's BDF by its incubator profile name. The
    // matching `requires(test_disk)` capability ensures it is provisioned
    // before the test runs.
    let vfio_bdf = incubator_vfio_bdf("test-disk")?;

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

/// Open the VFIO cdev for an assigned device given its PCI BDF.
///
/// Returns the opened character device file. The device must already be bound
/// to `vfio-pci` (the incubator does this before running the test).
fn open_vfio_cdev(vfio_bdf: &str) -> anyhow::Result<std::fs::File> {
    let sysfs_path = std::path::Path::new("/sys/bus/pci/devices").join(vfio_bdf);
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
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::path::Path::new("/dev/vfio/devices").join(cdev_name.file_name()))
        .context("failed to open VFIO cdev")
}

/// Look up the PCI BDF of an incubator-provisioned VFIO device by its profile
/// `name` (e.g. `"edu-initiator"`).
///
/// The incubator publishes each provisioned device's BDF in the environment as
/// `INCUBATOR_VFIO_BDF_<NAME>` (the name upper-cased with `-` replaced by `_`);
/// this derivation must match `setup_vfio_devices` in
/// `petri/incubator/src/qemu.rs`. Gate the test on the matching
/// `requires(...)` capability (the same name with `-` replaced by `_`) so the
/// device is guaranteed to be provisioned.
fn incubator_vfio_bdf(name: &str) -> anyhow::Result<String> {
    let env_name = format!(
        "INCUBATOR_VFIO_BDF_{}",
        name.to_uppercase().replace('-', "_")
    );
    std::env::var(&env_name).with_context(|| {
        format!("{env_name} not set; is device '{name}' provisioned by the incubator?")
    })
}

/// Validate device-BAR peer-to-peer DMA through the vfio-dmabuf import path.
///
/// This test assigns two emulated PCI devices from the incubator into the L2
/// OpenVMM guest, both under the *same* iommufd IOAS (`iommu0`):
///
/// - `edu` (QEMU's educational device) is the DMA **initiator**: it has a
///   register-programmed DMA engine that copies between its internal 4 KiB
///   buffer and an arbitrary bus address.
/// - `ivshmem-plain` is the DMA **target**: its BAR2 is a prefetchable,
///   RAM-backed memory BAR that serves as a peer-BAR DMA sink.
///
/// The guest (using only busybox `devmem`, no drivers) writes a pattern into
/// `ivshmem` BAR2, has `edu` DMA it into `edu`'s buffer (a peer-BAR *read*),
/// then DMA that buffer back out to a different `ivshmem` BAR2 offset (a
/// peer-BAR *write*), and finally reads that offset back to confirm the value
/// round-tripped through `edu`'s device-initiated DMA.
///
/// Because `ivshmem`'s BAR2 is device MMIO (not RAM), the only way `edu`'s DMA
/// can reach it is if OpenVMM imported `ivshmem`'s BAR dmabuf into the shared
/// IOAS (the Phase 2 vfio-dmabuf P2P path). Without that import the DMA faults
/// in the SMMU and the sink offset stays at its initialized sentinel, so a
/// matching read-back proves the dmabuf import engaged.
///
/// The `_aarch64_tcg` name suffix opts this test into the TCG incubator pass.
#[vmm_test_with(
    openvmm,
    requires(edu_initiator, ivshmem_target),
    configs(linux_direct_aarch64)
)]
async fn assigned_device_peer_to_peer_dma_aarch64_tcg(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    // BDFs of the two assigned devices, looked up by their incubator profile
    // names. The capability requirements above ensure both are provisioned
    // before the test runs.
    let edu_bdf = incubator_vfio_bdf("edu-initiator")?;
    let ivshmem_bdf = incubator_vfio_bdf("ivshmem-target")?;

    tracing::info!(%edu_bdf, %ivshmem_bdf, "assigning P2P device pair to guest");

    let edu_cdev = open_vfio_cdev(&edu_bdf)?;
    let ivshmem_cdev = open_vfio_cdev(&ivshmem_bdf)?;

    // Both devices must share one iommufd instance so they land in the same
    // IOAS (and thus the same dmabuf registry). Open /dev/iommu once and clone
    // the fd for the second device; OpenVMM dedups on the `iommu_id` string.
    let iommufd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/iommu")
        .context("failed to open /dev/iommu")?;
    let iommufd2 = iommufd
        .try_clone()
        .context("failed to clone /dev/iommu fd")?;

    let (edu_bdf_for_cfg, ivshmem_bdf_for_cfg) = (edu_bdf.clone(), ivshmem_bdf.clone());
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
                        pci_id: edu_bdf_for_cfg.clone(),
                        cdev: edu_cdev,
                        iommufd,
                        iommu_id: "iommu0".into(),
                        bar_pt: [false; 6],
                    }
                    .into_resource(),
                });
                c.pcie_devices.push(openvmm_defs::config::PcieDeviceConfig {
                    port_name: "s0rc0rp2".into(),
                    resource: vfio_assigned_device_resources::VfioCdevDeviceHandle {
                        pci_id: ivshmem_bdf_for_cfg.clone(),
                        cdev: ivshmem_cdev,
                        iommufd: iommufd2,
                        iommu_id: "iommu0".into(),
                        bar_pt: [false; 6],
                    }
                    .into_resource(),
                });
            })
        })
        .run()
        .await?;

    // Drive the P2P sequence from the test over pipette, one guest operation
    // at a time (no embedded shell script). edu BAR0 register map: 0x80 = DMA
    // source, 0x88 = DMA destination, 0x90 = transfer count (bytes), 0x98 = DMA
    // command (bit0 RUN, bit1 direction: 0 = bus -> edu buffer, 1 = edu buffer
    // -> bus). edu's internal buffer is addressed at 0x40000. BAR GPAs live in
    // the 64-bit high-MMIO window, so the DMA registers are written with 64-bit
    // accesses.
    const PATTERN: u64 = 0xDEAD_BEEF;
    const EDU_BUF: u64 = 0x40000;
    let sh = agent.unix_shell();

    // Parse a `0x`-prefixed (or bare) hex string into a `u64`.
    fn parse_hex_u64(s: &str) -> anyhow::Result<u64> {
        let t = s.trim();
        let hex = t.strip_prefix("0x").unwrap_or(t);
        u64::from_str_radix(hex, 16).with_context(|| format!("failed to parse hex {s:?}"))
    }

    // Write a value to guest physical memory via busybox `devmem`.
    let devmem_write = async |addr: u64, width: u32, val: u64| -> anyhow::Result<()> {
        let addr = format!("{addr:#x}");
        let width = width.to_string();
        let val = format!("{val:#x}");
        cmd!(sh, "devmem {addr} {width} {val}").run().await
    };

    // Read a value from guest physical memory via busybox `devmem`.
    let devmem_read = async |addr: u64, width: u32| -> anyhow::Result<u64> {
        let addr = format!("{addr:#x}");
        let width = width.to_string();
        let out = cmd!(sh, "devmem {addr} {width}").read().await?;
        parse_hex_u64(&out)
    };

    // Read the base GPA of a PCI BAR from sysfs (`resource` line index = BAR#).
    let pci_bar_base = async |bdf: &str, bar: usize| -> anyhow::Result<u64> {
        let resource = sh
            .read_file(format!("/sys/bus/pci/devices/{bdf}/resource"))
            .await?;
        let start = resource
            .lines()
            .nth(bar)
            .and_then(|l| l.split_whitespace().next())
            .with_context(|| format!("BAR {bar} missing in resource for {bdf}"))?;
        parse_hex_u64(start)
    };

    // Enable PCI memory-space decode + bus mastering by writing COMMAND =
    // MEM|BUSMASTER through sysfs config space. The device is freshly bound, so
    // a fixed write (rather than read/modify/write) is sufficient. 0x0006 =
    // MEM (bit1) | BUSMASTER (bit2), at the 16-bit COMMAND register (offset 4).
    let enable_mem_bus_master = async |bdf: &str| -> anyhow::Result<()> {
        let cfg = format!("/sys/bus/pci/devices/{bdf}/config");
        cmd!(sh, "dd of={cfg} bs=1 seek=4 count=2 conv=notrunc")
            .stdin([0x06u8, 0x00u8])
            .ignore_stdout()
            .run()
            .await
            .with_context(|| format!("failed to enable MEM|BUSMASTER on {bdf}"))
    };

    // Poll edu's DMA command register until the RUN bit (bit 0) clears, waiting
    // between polls (edu runs the transfer on a ~100ms timer).
    let wait_dma = async |reg_cmd: u64| -> anyhow::Result<()> {
        let mut timer = PolledTimer::new(&driver);
        for _ in 0..30 {
            if devmem_read(reg_cmd, 64).await? & 1 == 0 {
                return Ok(());
            }
            timer.sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("edu DMA RUN bit never cleared (peer-BAR DMA did not complete)")
    };

    // Enable memory-space decode + bus mastering on both devices so edu can
    // initiate DMA and ivshmem's BAR decodes.
    enable_mem_bus_master(&edu_bdf).await?;
    enable_mem_bus_master(&ivshmem_bdf).await?;

    // BAR base GPAs: edu BAR0 (registers) and ivshmem BAR2 (shared memory).
    let edu_bar0 = pci_bar_base(&edu_bdf, 0).await?;
    let ivs_bar2 = pci_bar_base(&ivshmem_bdf, 2).await?;
    tracing::info!("resolved BAR GPAs: edu_bar0={edu_bar0:#x} ivs_bar2={ivs_bar2:#x}");

    let reg_src = edu_bar0 + 0x80;
    let reg_dst = edu_bar0 + 0x88;
    let reg_cnt = edu_bar0 + 0x90;
    let reg_cmd = edu_bar0 + 0x98;
    let ivs_src = ivs_bar2;
    let ivs_sink = ivs_bar2 + 0x1000;

    // Sanity: edu BAR0 MMIO is live (identification register), so a later
    // failure is easy to triage.
    let edu_id = devmem_read(edu_bar0, 32).await?;
    anyhow::ensure!(
        edu_id == 0x0100_00ed,
        "edu identification register read {edu_id:#010x}, expected 0x010000ed"
    );

    // Seed the ivshmem source, clear the sink (CPU MMIO into ivshmem BAR2), and
    // confirm the seed landed.
    devmem_write(ivs_sink, 32, 0).await?;
    devmem_write(ivs_src, 32, PATTERN).await?;
    let seed = devmem_read(ivs_src, 32).await?;
    anyhow::ensure!(
        seed == PATTERN,
        "ivshmem BAR2 seed read {seed:#010x}, expected {PATTERN:#010x}"
    );

    // DMA #1 (FROM bus): P2P-read the ivshmem source into edu's buffer.
    devmem_write(reg_src, 64, ivs_src).await?;
    devmem_write(reg_dst, 64, EDU_BUF).await?;
    devmem_write(reg_cnt, 64, 4).await?;
    devmem_write(reg_cmd, 64, 1).await?; // RUN | FROM_PCI
    wait_dma(reg_cmd).await?;

    // DMA #2 (TO bus): P2P-write edu's buffer to the ivshmem sink offset.
    devmem_write(reg_src, 64, EDU_BUF).await?;
    devmem_write(reg_dst, 64, ivs_sink).await?;
    devmem_write(reg_cnt, 64, 4).await?;
    devmem_write(reg_cmd, 64, 3).await?; // RUN | TO_PCI
    wait_dma(reg_cmd).await?;

    // The pattern only reaches the sink if the Phase 2 dmabuf import routed
    // edu's peer-BAR DMA into ivshmem's BAR2 through the shared IOAS. Without
    // it the DMA faults in the IOAS and the sink stays zero.
    let result = devmem_read(ivs_sink, 32).await?;
    anyhow::ensure!(
        result == PATTERN,
        "peer-BAR P2P DMA did not round-trip the pattern: sink read {result:#010x}, \
         expected {PATTERN:#010x}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
