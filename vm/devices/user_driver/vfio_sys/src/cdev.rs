// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VFIO cdev (per-device fd) support.
//!
//! VFIO cdev is the modern device-access interface (`/dev/vfio/devices/vfioN`)
//! that replaces the legacy group/container model. Each device gets its own
//! character device node. The device is bound to an iommufd instance via
//! `VFIO_DEVICE_BIND_IOMMUFD`, and DMA is configured by attaching an iommufd
//! IOAS or HWPT via `VFIO_DEVICE_ATTACH_IOMMUFD_PT`.
//!
//! Once bound and attached, the device fd supports the same `VFIO_DEVICE_*`
//! ioctls as the legacy group path (get_info, get_region_info, set_irqs,
//! reset, mmap). The [`CdevDevice`] type wraps the fd and provides these
//! operations, producing a [`super::Device`] for the common ioctl surface.

use anyhow::Context as _;
use std::fs;
use std::os::unix::prelude::*;

mod ioctl {
    use nix::request_code_none;
    use vfio_bindings::bindings::vfio::VFIO_BASE;
    use vfio_bindings::bindings::vfio::VFIO_TYPE;

    // VFIO_DEVICE_BIND_IOMMUFD = _IO(VFIO_TYPE, VFIO_BASE + 18)
    nix::ioctl_readwrite_bad!(
        vfio_device_bind_iommufd,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 18),
        super::VfioDeviceBindIommufd
    );

    // VFIO_DEVICE_ATTACH_IOMMUFD_PT = _IO(VFIO_TYPE, VFIO_BASE + 19)
    nix::ioctl_readwrite_bad!(
        vfio_device_attach_iommufd_pt,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 19),
        super::VfioDeviceAttachIommufdPt
    );

    // VFIO_DEVICE_DETACH_IOMMUFD_PT = _IO(VFIO_TYPE, VFIO_BASE + 20)
    nix::ioctl_readwrite_bad!(
        vfio_device_detach_iommufd_pt,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 20),
        super::VfioDeviceDetachIommufdPt
    );
}

// Kernel ABI structs — must match `include/uapi/linux/vfio.h` exactly.

#[repr(C)]
pub struct VfioDeviceBindIommufd {
    pub argsz: u32,
    pub flags: u32,
    pub iommufd: i32,
    pub out_devid: u32,
}

#[repr(C)]
pub struct VfioDeviceAttachIommufdPt {
    pub argsz: u32,
    pub flags: u32,
    pub pt_id: u32,
}

#[repr(C)]
pub struct VfioDeviceDetachIommufdPt {
    pub argsz: u32,
    pub flags: u32,
}

/// A VFIO device opened via the cdev interface (`/dev/vfio/devices/vfioN`).
///
/// This is the modern per-device access path. After opening, the device must
/// be bound to an iommufd fd via [`bind_iommufd`](Self::bind_iommufd) and
/// then attached to an IOAS or HWPT via [`attach_ioas`](Self::attach_ioas)
/// before any DMA can occur.
///
/// Once bound and attached, call [`into_device`](Self::into_device) to get
/// the standard [`Device`](super::Device) for config space, BAR, IRQ, and
/// mmap operations.
pub struct CdevDevice {
    file: fs::File,
}

impl CdevDevice {
    /// Open a VFIO cdev device by its sysfs PCI address.
    ///
    /// Looks up the device's cdev node via
    /// `/sys/bus/pci/devices/<pci_id>/vfio-dev/` and opens the corresponding
    /// `/dev/vfio/devices/vfioN` character device.
    pub fn open(pci_id: &str) -> anyhow::Result<Self> {
        let vfio_dev_dir = std::path::Path::new("/sys/bus/pci/devices")
            .join(pci_id)
            .join("vfio-dev");

        // The vfio-dev/ directory contains a single entry like "vfio0"
        let entry = fs::read_dir(&vfio_dev_dir)
            .with_context(|| {
                format!(
                    "failed to read {}: is the device bound to vfio-pci?",
                    vfio_dev_dir.display()
                )
            })?
            .next()
            .context("no vfio-dev entry found")?
            .context("failed to read vfio-dev entry")?;

        let dev_name = entry.file_name();
        let dev_path = std::path::Path::new("/dev/vfio/devices").join(&dev_name);

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev_path)
            .with_context(|| format!("failed to open {}", dev_path.display()))?;

        Ok(Self { file })
    }

    /// Wrap a pre-opened VFIO cdev file descriptor.
    pub fn from_file(file: fs::File) -> Self {
        Self { file }
    }

    /// Bind this device to an iommufd instance.
    ///
    /// Returns the kernel-assigned device ID within the iommufd context.
    /// This must be called before any DMA operations.
    pub fn bind_iommufd(&self, iommufd_fd: RawFd) -> anyhow::Result<u32> {
        let mut cmd = VfioDeviceBindIommufd {
            argsz: size_of::<VfioDeviceBindIommufd>() as u32,
            flags: 0,
            iommufd: iommufd_fd,
            out_devid: 0,
        };
        // SAFETY: Both fds are valid, struct correctly constructed.
        unsafe {
            ioctl::vfio_device_bind_iommufd(self.file.as_raw_fd(), &mut cmd)
                .context("VFIO_DEVICE_BIND_IOMMUFD failed")?;
        }
        Ok(cmd.out_devid)
    }

    /// Attach the device to an IOAS or HWPT by its iommufd object ID.
    ///
    /// For Phase 4 (identity DMA), pass an IOAS ID. For Phase 5+ (nested
    /// translation), pass a HWPT ID.
    ///
    /// Returns the attached page table ID (may differ from input if the
    /// kernel auto-created a HWPT for the IOAS).
    pub fn attach_ioas(&self, pt_id: u32) -> anyhow::Result<u32> {
        let mut cmd = VfioDeviceAttachIommufdPt {
            argsz: size_of::<VfioDeviceAttachIommufdPt>() as u32,
            flags: 0,
            pt_id,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::vfio_device_attach_iommufd_pt(self.file.as_raw_fd(), &mut cmd)
                .context("VFIO_DEVICE_ATTACH_IOMMUFD_PT failed")?;
        }
        Ok(cmd.pt_id)
    }

    /// Detach the device from its current IOAS/HWPT.
    ///
    /// After detaching, the device is in a blocking DMA state.
    pub fn detach_ioas(&self) -> anyhow::Result<()> {
        let mut cmd = VfioDeviceDetachIommufdPt {
            argsz: size_of::<VfioDeviceDetachIommufdPt>() as u32,
            flags: 0,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::vfio_device_detach_iommufd_pt(self.file.as_raw_fd(), &mut cmd)
                .context("VFIO_DEVICE_DETACH_IOMMUFD_PT failed")?;
        }
        Ok(())
    }

    /// Convert to a standard [`Device`](super::Device) for config space,
    /// BAR, IRQ, and mmap operations.
    ///
    /// The cdev fd supports the same `VFIO_DEVICE_*` ioctls as the legacy
    /// group path, so the [`Device`](super::Device) type works unchanged.
    pub fn into_device(self) -> super::Device {
        super::Device { file: self.file }
    }
}

impl AsRef<fs::File> for CdevDevice {
    fn as_ref(&self) -> &fs::File {
        &self.file
    }
}

impl AsFd for CdevDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
