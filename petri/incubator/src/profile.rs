// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator profile definitions.

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

/// An incubator profile describing the backend platform and how to run it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncubatorProfile {
    /// Incubator backend configuration.
    pub incubator: IncubatorBackend,
    /// Guest boot environment, independent of the VM launcher.
    #[serde(default)]
    pub boot: IncubatorBoot,
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
    /// Native OpenVMM launcher.
    Openvmm(OpenvmmConfig),
}

/// Guest boot environment.
#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum IncubatorBoot {
    /// Boot a Linux kernel and prepared initrd directly.
    #[default]
    LinuxDirect,
    /// Launch the Microsoft hypervisor and its Linux root partition via UEFI.
    Mshv(MshvBootConfig),
}

/// Artifacts for mshv boot, independent of the launcher executable.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct MshvBootConfig {
    /// Compatible x64 UEFI firmware.
    pub firmware: std::path::PathBuf,
    /// Extracted artifact directory containing `boot/`.
    pub boot_artifacts: std::path::PathBuf,
}

impl IncubatorBackend {
    /// The guest architecture this backend emulates.
    pub fn arch(&self) -> Arch {
        match self {
            IncubatorBackend::QemuTcg(config) => config.arch,
            IncubatorBackend::Openvmm(config) => config.arch,
        }
    }
}

/// Native OpenVMM launcher and machine configuration.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OpenvmmConfig {
    /// Guest architecture (must match the native host).
    pub arch: Arch,
    /// Native host OpenVMM executable, independent of the guest test build.
    pub binary: std::path::PathBuf,
    /// Outer VM memory size.
    pub memory: String,
    /// Outer VM processor count.
    pub processors: u32,
    /// Expose nested virtualization. Required for mshv boot.
    #[serde(default)]
    pub nested_virt: bool,
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
#[serde(deny_unknown_fields)]
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
    /// `rdinit=/incubator-init.sh` (the injected init script); everything else,
    /// including the arch-specific serial console (e.g., "console=ttyAMA0"
    /// for aarch64 PL011, "console=ttyS0" for x86 16550), comes from here.
    pub cmdline: String,
}

impl IncubatorProfile {
    /// Reject unsupported launcher/boot combinations before reading artifacts.
    pub fn validate(&self) -> anyhow::Result<()> {
        match (&self.incubator, &self.boot) {
            (IncubatorBackend::QemuTcg(_), IncubatorBoot::Mshv(_)) => {
                anyhow::bail!("mshv boot is not yet supported by the qemu-tcg launcher");
            }
            (IncubatorBackend::Openvmm(config), IncubatorBoot::Mshv(_)) => {
                anyhow::ensure!(config.arch == Arch::X86_64, "mshv boot requires x86-64");
                anyhow::ensure!(config.nested_virt, "mshv boot requires nested-virt = true");
            }
            (_, IncubatorBoot::LinuxDirect) => {}
        }
        match &self.incubator {
            IncubatorBackend::QemuTcg(config) => {
                anyhow::ensure!(
                    config.arch == Arch::Aarch64,
                    "Petri QEMU currently supports only AArch64 guests"
                );
            }
            IncubatorBackend::Openvmm(config) => {
                anyhow::ensure!(
                    config.arch == Arch::X86_64,
                    "OpenVMM Incubator currently supports only x86-64 guests"
                );
                anyhow::ensure!(
                    config.processors > 0,
                    "OpenVMM requires at least one processor"
                );
                anyhow::ensure!(
                    parse_size(&config.memory)? > 0,
                    "OpenVMM memory must be nonzero"
                );
                anyhow::ensure!(
                    self.devices.is_empty(),
                    "extra profile devices are currently supported only by qemu-tcg"
                );
            }
        }
        Ok(())
    }

    /// Load a profile from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let contents = fs_err::read_to_string(path).context("failed to read incubator profile")?;
        Self::from_toml(&contents)
    }

    /// Parse a profile from a TOML string.
    pub fn from_toml(toml: &str) -> anyhow::Result<Self> {
        toml_edit::de::from_str(toml).context("failed to parse incubator profile")
    }
}

/// Parse memory/device sizes using the OpenVMM suffix convention.
pub(crate) fn parse_size(s: &str) -> anyhow::Result<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const OPENVMM: &str = include_str!("../profiles/x86_64-openvmm.toml");
    const MSHV: &str = include_str!("../profiles/x86_64-mshv.toml");
    const QEMU: &str = include_str!("../profiles/aarch64-tcg-pcie.toml");

    #[test]
    fn launcher_and_boot_are_separate() -> anyhow::Result<()> {
        for profile in [OPENVMM, MSHV, QEMU] {
            IncubatorProfile::from_toml(profile)?.validate()?;
        }
        let direct = IncubatorProfile::from_toml(OPENVMM)?;
        assert!(matches!(direct.incubator, IncubatorBackend::Openvmm(_)));
        assert!(matches!(direct.boot, IncubatorBoot::LinuxDirect));
        let mshv = IncubatorProfile::from_toml(MSHV)?;
        assert!(matches!(mshv.incubator, IncubatorBackend::Openvmm(_)));
        assert!(matches!(mshv.boot, IncubatorBoot::Mshv(_)));
        let qemu = IncubatorProfile::from_toml(QEMU)?;
        assert!(matches!(qemu.boot, IncubatorBoot::LinuxDirect));
        Ok(())
    }

    #[test]
    fn unsupported_combinations_and_misplaced_fields_fail() -> anyhow::Result<()> {
        let qemu_mshv =
            format!("{QEMU}\n[boot]\ntype = 'mshv'\nfirmware = 'fw'\nboot-artifacts = 'boot'\n");
        let error = IncubatorProfile::from_toml(&qemu_mshv)?
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("not yet supported"), "{error:#}");
        for invalid in [
            MSHV.replace("nested-virt = true", "nested-virt = false"),
            MSHV.replace("x86-64", "aarch64"),
            OPENVMM.replace("processors = 2", "processors = 0"),
        ] {
            assert!(IncubatorProfile::from_toml(&invalid)?.validate().is_err());
        }
        assert!(
            IncubatorProfile::from_toml(
                &OPENVMM.replace("[boot]", "boot-artifacts = 'wrong-section'\n[boot]")
            )
            .is_err()
        );
        assert!(IncubatorProfile::from_toml(&OPENVMM.replace("linux-direct", "typo")).is_err());
        Ok(())
    }
}
