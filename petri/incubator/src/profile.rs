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

impl IncubatorBackend {
    /// The guest architecture this backend emulates.
    pub fn arch(&self) -> Arch {
        match self {
            IncubatorBackend::QemuTcg(config) => config.arch,
        }
    }
}

/// Guest architecture emulated by an incubator backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    /// x86-64.
    X86_64,
    /// AArch64.
    Aarch64,
}

impl Arch {
    /// The prefix used for arch-specific environment variables, matching
    /// openvmm's convention (e.g., `X86_64_OPENVMM_LINUX_DIRECT_KERNEL`).
    pub fn env_prefix(self) -> &'static str {
        match self {
            Arch::X86_64 => "X86_64",
            Arch::Aarch64 => "AARCH64",
        }
    }
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
    pub size: String,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

/// QEMU TCG configuration parsed from the profile.
#[derive(Debug, Clone, Deserialize)]
pub struct QemuTcgConfig {
    /// Guest architecture (e.g., "aarch64", "x86-64"). Selects the
    /// arch-specific kernel/initrd when those are auto-detected.
    pub arch: Arch,
    /// Path or name of the QEMU binary (e.g., "qemu-system-aarch64").
    pub binary: String,
    /// Machine type (e.g., "virt,virtualization=on,iommu=smmuv3").
    pub machine: String,
    /// CPU model (e.g., "max").
    pub cpu: String,
    /// Memory size (e.g., "4G").
    pub memory: String,
    /// Number of CPUs (e.g., "2").
    pub smp: String,
    /// Extra kernel command line arguments. The incubator always appends
    /// `rdinit=/tcg-init.sh` (the injected init script); everything else,
    /// including the arch-specific serial console (e.g., "console=ttyAMA0"
    /// for aarch64 PL011, "console=ttyS0" for x86 16550), comes from here.
    pub cmdline: String,
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
