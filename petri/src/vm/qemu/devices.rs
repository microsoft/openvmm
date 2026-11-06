// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU device configurations.
// TODO: should some/all of these be backend agnostic?

use std::path::PathBuf;

/// Backing storage for a virtio-blk device.
#[derive(Debug, Clone)]
pub enum VirtioBlkBacking {
    /// Discard writes and return zeroes on reads, with the specified size.
    Null(u64),
    /// A raw file, accessed through io_uring.
    File {
        /// Host path to the raw disk image.
        path: PathBuf,
        /// Whether the disk is read-only.
        read_only: bool,
        /// Bypass the host page cache.
        direct: bool,
    },
}

/// Optional virtio-blk device properties. Unspecified properties use QEMU defaults.
#[derive(Debug, Clone, Default)]
pub struct VirtioBlkOptions {
    /// Advertise packed virtqueues.
    pub packed: Option<bool>,
    /// Number of descriptors in each virtqueue.
    pub queue_size: Option<u16>,
    /// Number of request virtqueues.
    pub num_queues: Option<u16>,
    /// Allow QEMU to merge adjacent requests.
    pub request_merging: Option<bool>,
    /// Disable the legacy virtio PCI interface.
    pub disable_legacy: Option<bool>,
    /// Require the guest to use platform DMA translation.
    pub iommu_platform: Option<bool>,
    /// Logical sector size in bytes.
    pub logical_block_size: Option<u32>,
    /// Physical sector size in bytes.
    pub physical_block_size: Option<u32>,
}

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
    /// Storage backing this device.
    pub backing: VirtioBlkBacking,
    /// Device properties; unspecified properties use QEMU defaults.
    pub options: VirtioBlkOptions,
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
