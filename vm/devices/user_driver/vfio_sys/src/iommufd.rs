// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for the Linux iommufd subsystem (`/dev/iommu`).
//!
//! Provides safe wrappers around `IOMMU_IOAS_ALLOC`, `IOMMU_IOAS_MAP`,
//! `IOMMU_IOAS_MAP_FILE`, `IOMMU_IOAS_UNMAP`, and `IOMMU_DESTROY` ioctls,
//! which together support identity DMA mapping via an IOAS.

use anyhow::Context as _;
use std::fs;
use std::os::unix::prelude::*;

mod ioctl {
    use nix::request_code_none;

    /// iommufd ioctl type character (';' = 0x3B).
    const IOMMUFD_TYPE: u8 = b';';

    /// Base command number for iommufd ioctls.
    const IOMMUFD_CMD_BASE: u8 = 0x80;

    // Command numbers (IOMMUFD_CMD_BASE + offset).
    const IOMMUFD_CMD_DESTROY: u8 = IOMMUFD_CMD_BASE;
    const IOMMUFD_CMD_IOAS_ALLOC: u8 = IOMMUFD_CMD_BASE + 1;
    const IOMMUFD_CMD_IOAS_MAP: u8 = IOMMUFD_CMD_BASE + 5;
    const IOMMUFD_CMD_IOAS_UNMAP: u8 = IOMMUFD_CMD_BASE + 6;
    const IOMMUFD_CMD_IOAS_MAP_FILE: u8 = IOMMUFD_CMD_BASE + 15;

    /// Flags for `IOMMU_IOAS_MAP`.
    pub(super) const IOMMU_IOAS_MAP_FIXED_IOVA: u32 = 1 << 0;
    pub(super) const IOMMU_IOAS_MAP_WRITEABLE: u32 = 1 << 1;
    pub(super) const IOMMU_IOAS_MAP_READABLE: u32 = 1 << 2;

    // IOMMUFD ioctls use _IO (no direction, just type + nr).
    // The kernel defines them as _IO(IOMMUFD_TYPE, cmd_nr).
    nix::ioctl_readwrite_bad!(
        iommu_destroy,
        request_code_none!(IOMMUFD_TYPE as u32, IOMMUFD_CMD_DESTROY as u32),
        IommuDestroy
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_alloc,
        request_code_none!(IOMMUFD_TYPE as u32, IOMMUFD_CMD_IOAS_ALLOC as u32),
        IommuIoasAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_map,
        request_code_none!(IOMMUFD_TYPE as u32, IOMMUFD_CMD_IOAS_MAP as u32),
        IommuIoasMap
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_map_file,
        request_code_none!(IOMMUFD_TYPE as u32, IOMMUFD_CMD_IOAS_MAP_FILE as u32),
        IommuIoasMapFile
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_unmap,
        request_code_none!(IOMMUFD_TYPE as u32, IOMMUFD_CMD_IOAS_UNMAP as u32),
        IommuIoasUnmap
    );

    // Kernel ABI structs — must match `include/uapi/linux/iommufd.h` exactly.

    #[repr(C)]
    pub(super) struct IommuDestroy {
        pub(super) size: u32,
        pub(super) id: u32,
    }

    #[repr(C)]
    pub(super) struct IommuIoasAlloc {
        pub(super) size: u32,
        pub(super) flags: u32,
        pub(super) out_ioas_id: u32,
    }

    #[repr(C)]
    pub(super) struct IommuIoasMap {
        pub(super) size: u32,
        pub(super) flags: u32,
        pub(super) ioas_id: u32,
        pub(super) __reserved: u32,
        pub(super) user_va: u64,
        pub(super) length: u64,
        pub(super) iova: u64,
    }

    #[repr(C)]
    pub(super) struct IommuIoasMapFile {
        pub(super) size: u32,
        pub(super) flags: u32,
        pub(super) ioas_id: u32,
        pub(super) fd: i32,
        pub(super) start: u64,
        pub(super) length: u64,
        pub(super) iova: u64,
    }

    #[repr(C)]
    pub(super) struct IommuIoasUnmap {
        pub(super) size: u32,
        pub(super) ioas_id: u32,
        pub(super) iova: u64,
        pub(super) length: u64,
    }
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
        let mut cmd = ioctl::IommuIoasAlloc {
            size: size_of::<ioctl::IommuIoasAlloc>() as u32,
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
        let mut flags = ioctl::IOMMU_IOAS_MAP_FIXED_IOVA | ioctl::IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= ioctl::IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = ioctl::IommuIoasMap {
            size: size_of::<ioctl::IommuIoasMap>() as u32,
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

    /// Map a file/memfd range into an IOAS at a fixed IOVA via
    /// `IOMMU_IOAS_MAP_FILE`.
    ///
    /// Unlike [`Self::ioas_map`], the kernel pins the backing folios directly
    /// from `fd`, so no host VA is required. `start` is the byte offset within
    /// the file; like [`Self::ioas_map`], both `start` and `length` must be
    /// page-aligned. Requires a kernel with `IOMMU_IOAS_MAP_FILE` (Linux
    /// 6.13+).
    pub fn ioas_map_file(
        &self,
        ioas_id: u32,
        iova: u64,
        fd: RawFd,
        start: u64,
        length: u64,
        writable: bool,
    ) -> anyhow::Result<()> {
        let mut flags = ioctl::IOMMU_IOAS_MAP_FIXED_IOVA | ioctl::IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= ioctl::IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = ioctl::IommuIoasMapFile {
            size: size_of::<ioctl::IommuIoasMapFile>() as u32,
            flags,
            ioas_id,
            fd,
            start,
            length,
            iova,
        };
        // SAFETY: the iommufd fd is valid and the struct is correctly sized and
        // constructed. `fd` is only read during the ioctl.
        unsafe {
            ioctl::iommu_ioas_map_file(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_MAP_FILE failed")?;
        }
        Ok(())
    }

    /// Unmap an IOVA range from an IOAS.
    ///
    /// Returns the number of bytes actually unmapped.
    pub fn ioas_unmap(&self, ioas_id: u32, iova: u64, length: u64) -> anyhow::Result<u64> {
        let mut cmd = ioctl::IommuIoasUnmap {
            size: size_of::<ioctl::IommuIoasUnmap>() as u32,
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
        let mut cmd = ioctl::IommuDestroy {
            size: size_of::<ioctl::IommuDestroy>() as u32,
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
