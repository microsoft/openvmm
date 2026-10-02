// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU device configurations.
// TODO: should some/all of these be backend agnostic?

/// A device to add to the platform.
#[derive(Debug, Clone)]
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
    /// The device's name
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

/// Configuration for a virtio-blk device.
#[derive(Debug, Clone)]
pub struct VirtioBlkDeviceConfig {
    /// Name for this device
    pub name: String,
    /// Size of the RAM-backed disk in bytes.
    pub size: u64,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    pub vfio: bool,
}

/// Configuration for a QEMU `edu` device.
#[derive(Debug, Clone)]
pub struct EduDeviceConfig {
    /// Name for this device.
    pub name: String,
    /// Optional `dma_mask` for the edu DMA engine.
    /// The edu default is 28 bits, which clamps DMA addresses to the low
    /// 256 MiB — too small for aarch64 guest physical addresses, so P2P tests
    /// must widen it.
    pub dma_mask: Option<u64>,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    pub vfio: bool,
}

/// Configuration for a QEMU `ivshmem-plain`.
#[derive(Debug, Clone)]
pub struct IvshmemPlainDeviceConfig {
    /// Name for this device.
    pub name: String,
    /// Size of the RAM-backed shared-memory BAR2 in bytes.
    pub size: u64,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    pub vfio: bool,
}
