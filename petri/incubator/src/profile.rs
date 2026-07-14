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
    /// A QEMU `edu` device — a simple PCI device with a register-programmed
    /// DMA engine. Used as a P2P DMA *initiator* in device-assignment tests.
    Edu(EduDeviceConfig),
    /// A QEMU `ivshmem-plain` device — a PCI device whose BAR2 is a
    /// prefetchable, RAM-backed memory window. Used as a P2P DMA *target*
    /// (peer BAR) in device-assignment tests.
    IvshmemPlain(IvshmemPlainDeviceConfig),
}

impl DeviceConfig {
    /// The device's name, used in env var names (e.g. `test-disk` →
    /// `INCUBATOR_VFIO_BDF_TEST_DISK`).
    pub fn name(&self) -> &str {
        match self {
            DeviceConfig::VirtioBlk(cfg) => &cfg.name,
            DeviceConfig::Edu(cfg) => &cfg.name,
            DeviceConfig::IvshmemPlain(cfg) => &cfg.name,
        }
    }

    /// Whether the device should be bound to vfio-pci after boot so it can be
    /// assigned into the L2 guest.
    pub fn vfio(&self) -> bool {
        match self {
            DeviceConfig::VirtioBlk(cfg) => cfg.vfio,
            DeviceConfig::Edu(cfg) => cfg.vfio,
            DeviceConfig::IvshmemPlain(cfg) => cfg.vfio,
        }
    }

    /// The capability this device advertises once provisioned, derived from
    /// its name with `-` replaced by `_` so it is a valid `requires(...)`
    /// identifier (e.g. `edu-initiator` → `edu_initiator`). Tests gate on this
    /// via `requires(...)`.
    pub fn capability(&self) -> String {
        self.name().replace('-', "_")
    }
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

/// Configuration for a QEMU `edu` device added to the incubator.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EduDeviceConfig {
    /// Name for this device (used in env var names, e.g., "edu-initiator" →
    /// `INCUBATOR_VFIO_BDF_EDU_INITIATOR`).
    pub name: String,
    /// Optional `dma_mask` for the edu DMA engine (e.g. "0xffffffffffff").
    /// The edu default is 28 bits, which clamps DMA addresses to the low
    /// 256 MiB — too small for aarch64 guest physical addresses, so P2P tests
    /// must widen it. Accepts decimal or `0x`-prefixed hex.
    #[serde(default)]
    pub dma_mask: Option<String>,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

/// Configuration for a QEMU `ivshmem-plain` device added to the incubator.
#[derive(Debug, Deserialize)]
pub struct IvshmemPlainDeviceConfig {
    /// Name for this device (used in env var names, e.g., "ivshmem-target" →
    /// `INCUBATOR_VFIO_BDF_IVSHMEM_TARGET`).
    pub name: String,
    /// Size of the RAM-backed shared-memory BAR2 (e.g., "4M").
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
