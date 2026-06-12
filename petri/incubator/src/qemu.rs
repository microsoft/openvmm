// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU process management.

use crate::profile::DeviceConfig;
use crate::profile::QemuTcgConfig;
use anyhow::Context;
use std::path::Path;
use std::process::Command;

/// Build the QEMU command line for a TCG launch.
pub fn build_qemu_command(
    config: &QemuTcgConfig,
    devices: &[DeviceConfig],
    kernel: &Path,
    initrd: &Path,
    share_dir: &Path,
    host_pipette_port: u16,
    kernel_cmdline: &str,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new(&config.binary);

    cmd.arg("-machine").arg(&config.machine);
    cmd.arg("-cpu").arg(&config.cpu);
    cmd.arg("-m").arg(&config.memory);
    cmd.arg("-smp").arg(&config.smp);
    cmd.arg("-nographic");
    cmd.arg("-kernel").arg(kernel);
    cmd.arg("-initrd").arg(initrd);
    cmd.arg("-append").arg(kernel_cmdline);
    cmd.arg("-no-reboot");

    // 9p: share the host directory into the guest
    cmd.arg("-fsdev").arg(format!(
        "local,id=fsdev0,path={},security_model=none",
        share_dir.display()
    ));
    cmd.arg("-device")
        .arg("virtio-9p-pci,fsdev=fsdev0,mount_tag=hostshare");

    // User-mode networking with port forwarding for pipette TCP
    cmd.arg("-netdev").arg(format!(
        "user,id=net0,hostfwd=tcp::{host_pipette_port}-:{guest_port}",
        guest_port = pipette_client::PIPETTE_PORT,
    ));
    cmd.arg("-device")
        .arg("virtio-net-pci,netdev=net0,romfile=");

    // Console on serial (diagnostic only)
    cmd.arg("-serial").arg("mon:stdio");

    // Extra devices from the profile.
    // Each device gets its own PCIe root port at a known PCI device number
    // (`addr=`), so the VFIO setup code can find the bridge by its devfn
    // in sysfs and enumerate the child behind it.
    for (i, device) in devices.iter().enumerate() {
        let rp_id = format!("hosting_rp{i}");
        let addr = EXTRA_DEVICE_ADDR_BASE + i;
        cmd.arg("-device")
            .arg(format!("pcie-root-port,id={rp_id},addr={addr:#x}"));

        match device {
            DeviceConfig::VirtioBlk(cfg) => {
                let node_name = format!("disk{i}");
                let size_bytes = parse_size(&cfg.size)
                    .with_context(|| format!("invalid size for device '{}'", cfg.name))?;
                cmd.arg("-blockdev")
                    .arg(format!("null-co,node-name={node_name},size={size_bytes}"));
                cmd.arg("-device")
                    .arg(format!("virtio-blk-pci,drive={node_name},bus={rp_id},iommu_platform=on,disable-legacy=on,romfile="));
            }
        }
    }

    Ok(cmd)
}

/// First PCI device number (`addr=`) used for extra-device root ports.
///
/// QEMU's built-in devices use low device numbers. We start at 16 (0x10)
/// to avoid collisions. The root port for the i-th extra device has
/// devfn = `(EXTRA_DEVICE_ADDR_BASE + i) << 3`.
pub const EXTRA_DEVICE_ADDR_BASE: usize = 16;

/// Parse a human-readable size string (e.g., "64M", "1G", "512K") to bytes.
///
/// Accepts the suffixes K, M, G, and T, optionally followed by B. A plain
/// integer (no suffix) is interpreted as a byte count.
//
// Copied from openvmm's CLI memory parser (`parse_memory` in
// openvmm_entry::cli_args) to keep behavior consistent without taking a
// dependency just to share this small helper.
fn parse_size(s: &str) -> anyhow::Result<u64> {
    || -> Option<u64> {
        let mut b = s.as_bytes();
        if s.ends_with('B') {
            b = &b[..b.len() - 1]
        }
        if b.is_empty() {
            return None;
        }
        let multi = match b[b.len() - 1] as char {
            'T' => Some(1024 * 1024 * 1024 * 1024),
            'G' => Some(1024 * 1024 * 1024),
            'M' => Some(1024 * 1024),
            'K' => Some(1024),
            _ => None,
        };
        if multi.is_some() {
            b = &b[..b.len() - 1]
        }
        let n: u64 = std::str::from_utf8(b).ok()?.parse().ok()?;
        n.checked_mul(multi.unwrap_or(1))
    }()
    .with_context(|| format!("invalid size '{s}'"))
}
