// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator profile definitions.

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

/// An incubator profile describing the backend platform and how to run it.
#[derive(Debug, Deserialize)]
pub struct IncubatorProfile {
    /// Incubator backend configuration.
    pub incubator: IncubatorBackend,
    /// Extra devices to add to the platform.
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

/// Backend-specific configuration, tagged by `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum IncubatorBackend {
    /// QEMU TCG emulation.
    QemuTcg(QemuTcgConfig),
}

/// A device to add to the platform.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DeviceConfig {
    /// A virtio-blk disk device.
    VirtioBlk(VirtioBlkDeviceConfig),
}

/// Configuration for a virtio-blk device added to the incubator.
#[derive(Debug, Deserialize)]
pub struct VirtioBlkDeviceConfig {
    /// Name for this device (used in env var names, e.g., "test-disk" →
    /// `INCUBATOR_VFIO_BDF_TEST_DISK`).
    pub name: String,
    /// Size of the RAM-backed disk (e.g., "64M").
    #[serde(default = "default_disk_size")]
    pub size: String,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

fn default_disk_size() -> String {
    "64M".to_string()
}

/// QEMU TCG configuration parsed from the profile.
#[derive(Debug, Clone, Deserialize)]
pub struct QemuTcgConfig {
    /// Path or name of the QEMU binary (e.g., "qemu-system-aarch64").
    #[serde(default = "default_qemu_binary")]
    pub binary: String,
    /// Machine type (e.g., "virt,virtualization=on,iommu=smmuv3").
    #[serde(default = "default_machine")]
    pub machine: String,
    /// CPU model (e.g., "max").
    #[serde(default = "default_cpu")]
    pub cpu: String,
    /// Memory size (e.g., "4G").
    #[serde(default = "default_memory")]
    pub memory: String,
    /// Number of CPUs (e.g., "2").
    #[serde(default = "default_smp")]
    pub smp: String,
}

fn default_qemu_binary() -> String {
    "qemu-system-aarch64".to_string()
}
fn default_machine() -> String {
    "virt".to_string()
}
fn default_cpu() -> String {
    "max".to_string()
}
fn default_memory() -> String {
    "4G".to_string()
}
fn default_smp() -> String {
    "2".to_string()
}

impl IncubatorProfile {
    /// Load a profile from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path).context("failed to read incubator profile")?;
        Self::from_toml(&contents)
    }

    /// Parse a profile from a TOML string.
    pub fn from_toml(toml: &str) -> anyhow::Result<Self> {
        toml_edit::de::from_str(toml).context("failed to parse incubator profile")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aarch64_pcie_profile() {
        let toml = r#"
[incubator]
type = "qemu-tcg"
binary = "qemu-system-aarch64"
machine = "virt,virtualization=on,iommu=smmuv3,gic-version=3"
cpu = "max"
memory = "4G"
smp = "2"

[[devices]]
type = "virtio-blk"
name = "test-disk"
size = "64M"
vfio = true
"#;
        let profile = IncubatorProfile::from_toml(toml).unwrap();
        match &profile.incubator {
            IncubatorBackend::QemuTcg(cfg) => {
                assert_eq!(
                    cfg.machine,
                    "virt,virtualization=on,iommu=smmuv3,gic-version=3"
                );
                assert_eq!(cfg.cpu, "max");
            }
        }
        assert_eq!(profile.devices.len(), 1);
        match &profile.devices[0] {
            DeviceConfig::VirtioBlk(cfg) => {
                assert_eq!(cfg.name, "test-disk");
                assert_eq!(cfg.size, "64M");
                assert!(cfg.vfio);
            }
        }
    }
}
