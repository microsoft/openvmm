// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for the Linux iommufd subsystem (`/dev/iommu`).
//!
//! Provides safe wrappers around `IOMMU_IOAS_ALLOC`, `IOMMU_IOAS_MAP`,
//! `IOMMU_IOAS_UNMAP`, and `IOMMU_DESTROY` ioctls, which together support
//! identity DMA mapping via an IOAS.

use anyhow::Context as _;
use std::fs;
use std::os::unix::prelude::*;

/// iommufd ioctl type character (';' = 0x3B).
const IOMMUFD_TYPE: u8 = b';';

/// Base command number for iommufd ioctls.
const IOMMUFD_CMD_BASE: u8 = 0x80;

// Command numbers (IOMMUFD_CMD_BASE + offset).
const IOMMUFD_CMD_DESTROY: u8 = IOMMUFD_CMD_BASE;
const IOMMUFD_CMD_IOAS_ALLOC: u8 = IOMMUFD_CMD_BASE + 1;
const IOMMUFD_CMD_IOAS_MAP: u8 = IOMMUFD_CMD_BASE + 5;
const IOMMUFD_CMD_IOAS_UNMAP: u8 = IOMMUFD_CMD_BASE + 6;

/// Flags for `IOMMU_IOAS_MAP`.
pub const IOMMU_IOAS_MAP_FIXED_IOVA: u32 = 1 << 0;
pub const IOMMU_IOAS_MAP_WRITEABLE: u32 = 1 << 1;
pub const IOMMU_IOAS_MAP_READABLE: u32 = 1 << 2;

mod ioctl {
    use nix::request_code_none;

    // IOMMUFD ioctls use _IO (no direction, just type + nr).
    // The kernel defines them as _IO(IOMMUFD_TYPE, cmd_nr).
    nix::ioctl_readwrite_bad!(
        iommu_destroy,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_DESTROY as u32
        ),
        super::IommuDestroy
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_ALLOC as u32
        ),
        super::IommuIoasAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_map,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_MAP as u32
        ),
        super::IommuIoasMap
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_unmap,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_UNMAP as u32
        ),
        super::IommuIoasUnmap
    );
}

// Kernel ABI structs — must match `include/uapi/linux/iommufd.h` exactly.

#[repr(C)]
pub struct IommuDestroy {
    pub size: u32,
    pub id: u32,
}

#[repr(C)]
pub struct IommuIoasAlloc {
    pub size: u32,
    pub flags: u32,
    pub out_ioas_id: u32,
}

#[repr(C)]
pub struct IommuIoasMap {
    pub size: u32,
    pub flags: u32,
    pub ioas_id: u32,
    pub __reserved: u32,
    pub user_va: u64,
    pub length: u64,
    pub iova: u64,
}

#[repr(C)]
pub struct IommuIoasUnmap {
    pub size: u32,
    pub ioas_id: u32,
    pub iova: u64,
    pub length: u64,
}

/// An open iommufd file descriptor (`/dev/iommu`).
///
/// Wraps the fd and provides safe methods for the iommufd ioctls needed
/// to allocate an IOAS and map/unmap host memory into it.
pub struct IommufdCtx {
    file: fs::File,
}

impl IommufdCtx {
    /// Open `/dev/iommu` and return a new iommufd context.
    pub fn new() -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/iommu")
            .context("failed to open /dev/iommu")?;
        Ok(Self { file })
    }

    /// Wrap an existing iommufd file descriptor.
    pub fn from_file(file: fs::File) -> Self {
        Self { file }
    }

    /// Allocate a new IO Address Space (IOAS).
    ///
    /// Returns the kernel-assigned IOAS object ID.
    pub fn ioas_alloc(&self) -> anyhow::Result<u32> {
        let mut cmd = IommuIoasAlloc {
            size: size_of::<IommuIoasAlloc>() as u32,
            flags: 0,
            out_ioas_id: 0,
        };
        // SAFETY: fd is valid, struct is correctly sized and zeroed.
        unsafe {
            ioctl::iommu_ioas_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_ALLOC failed")?;
        }
        Ok(cmd.out_ioas_id)
    }

    /// Map a user VA range into an IOAS at a fixed IOVA.
    ///
    /// `ioas_id` is the IOAS to map into. `iova` is the fixed IO virtual
    /// address. `user_va` is the host virtual address of the backing memory.
    /// `length` is the size in bytes (must be page-aligned).
    ///
    /// # Safety
    /// `user_va` must point to valid, backed memory for `length` bytes.
    /// The memory must remain mapped for the lifetime of this IOAS mapping.
    pub unsafe fn ioas_map(
        &self,
        ioas_id: u32,
        iova: u64,
        user_va: u64,
        length: u64,
        writable: bool,
    ) -> anyhow::Result<()> {
        let mut flags = IOMMU_IOAS_MAP_FIXED_IOVA | IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = IommuIoasMap {
            size: size_of::<IommuIoasMap>() as u32,
            flags,
            ioas_id,
            __reserved: 0,
            user_va,
            length,
            iova,
        };
        // SAFETY: fd is valid, struct correctly constructed. Caller
        // guarantees user_va is backed and stable.
        unsafe {
            ioctl::iommu_ioas_map(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_MAP failed")?;
        }
        Ok(())
    }

    /// Unmap an IOVA range from an IOAS.
    ///
    /// Returns the number of bytes actually unmapped.
    pub fn ioas_unmap(&self, ioas_id: u32, iova: u64, length: u64) -> anyhow::Result<u64> {
        let mut cmd = IommuIoasUnmap {
            size: size_of::<IommuIoasUnmap>() as u32,
            ioas_id,
            iova,
            length,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_ioas_unmap(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_UNMAP failed")?;
        }
        Ok(cmd.length)
    }

    /// Destroy an iommufd object by its ID.
    pub fn destroy(&self, id: u32) -> anyhow::Result<()> {
        let mut cmd = IommuDestroy {
            size: size_of::<IommuDestroy>() as u32,
            id,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_destroy(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_DESTROY failed")?;
        }
        Ok(())
    }
}

impl AsFd for IommufdCtx {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl AsRawFd for IommufdCtx {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
