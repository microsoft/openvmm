// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for the Linux iommufd subsystem (`/dev/iommu`).
//!
//! Provides safe wrappers around iommufd ioctls for:
//! - IOAS allocation and DMA mapping (`IOMMU_IOAS_ALLOC`, `IOMMU_IOAS_MAP`,
//!   `IOMMU_IOAS_MAP_FILE`, `IOMMU_IOAS_UNMAP`)
//! - Hardware page table management (`IOMMU_HWPT_ALLOC`, `IOMMU_HWPT_INVALIDATE`)
//! - Hardware info query (`IOMMU_GET_HW_INFO`)
//! - Virtual IOMMU objects (`IOMMU_VIOMMU_ALLOC`, `IOMMU_VDEVICE_ALLOC`,
//!   `IOMMU_VEVENTQ_ALLOC`)
//!
//! The IOAS path supports identity DMA mapping (Phase 4). The HWPT/vIOMMU
//! path supports nested stage 1 translation for VFIO passthrough (Phase 5).

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
const IOMMUFD_CMD_HWPT_ALLOC: u8 = IOMMUFD_CMD_BASE + 9;
const IOMMUFD_CMD_IOAS_MAP_FILE: u8 = IOMMUFD_CMD_BASE + 15;
const IOMMUFD_CMD_GET_HW_INFO: u8 = IOMMUFD_CMD_BASE + 0x0a;
const IOMMUFD_CMD_HWPT_INVALIDATE: u8 = IOMMUFD_CMD_BASE + 0x0d;
const IOMMUFD_CMD_VIOMMU_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x10;
const IOMMUFD_CMD_VDEVICE_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x11;
const IOMMUFD_CMD_VEVENTQ_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x13;

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
        iommu_ioas_map_file,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_MAP_FILE as u32
        ),
        super::IommuIoasMapFile
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_unmap,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_UNMAP as u32
        ),
        super::IommuIoasUnmap
    );
    nix::ioctl_readwrite_bad!(
        iommu_hwpt_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_HWPT_ALLOC as u32
        ),
        super::IommuHwptAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_get_hw_info,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_GET_HW_INFO as u32
        ),
        super::IommuGetHwInfo
    );
    nix::ioctl_readwrite_bad!(
        iommu_hwpt_invalidate,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_HWPT_INVALIDATE as u32
        ),
        super::IommuHwptInvalidate
    );
    nix::ioctl_readwrite_bad!(
        iommu_viommu_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VIOMMU_ALLOC as u32
        ),
        super::IommuViommuAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_vdevice_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VDEVICE_ALLOC as u32
        ),
        super::IommuVdeviceAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_veventq_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VEVENTQ_ALLOC as u32
        ),
        super::IommuVeventqAlloc
    );
}

// Kernel ABI structs — must match `include/uapi/linux/iommufd.h` exactly.

#[repr(C)]
struct IommuDestroy {
    size: u32,
    id: u32,
}

#[repr(C)]
struct IommuIoasAlloc {
    size: u32,
    flags: u32,
    out_ioas_id: u32,
}

#[repr(C)]
struct IommuIoasMap {
    size: u32,
    flags: u32,
    ioas_id: u32,
    __reserved: u32,
    user_va: u64,
    length: u64,
    iova: u64,
}

#[repr(C)]
struct IommuIoasMapFile {
    size: u32,
    flags: u32,
    ioas_id: u32,
    fd: i32,
    start: u64,
    length: u64,
    iova: u64,
}

#[repr(C)]
struct IommuIoasUnmap {
    size: u32,
    ioas_id: u32,
    iova: u64,
    length: u64,
}

// --- HWPT allocation ---

/// Flags for `IOMMU_HWPT_ALLOC`.
pub const IOMMU_HWPT_ALLOC_NEST_PARENT: u32 = 1 << 0;

/// HWPT data type: no type-specific data.
pub const IOMMU_HWPT_DATA_NONE: u32 = 0;
/// HWPT data type: ARM SMMUv3 (nested STE DW0-1).
pub const IOMMU_HWPT_DATA_ARM_SMMUV3: u32 = 2;

#[repr(C)]
struct IommuHwptAlloc {
    size: u32,
    flags: u32,
    dev_id: u32,
    pt_id: u32,
    out_hwpt_id: u32,
    __reserved: u32,
    data_type: u32,
    data_len: u32,
    data_uptr: u64,
    fault_id: u32,
    __reserved2: u32,
}

/// ARM SMMUv3 nested HWPT data: the first two double words of the STE.
///
/// Passed via `data_uptr` when `data_type == IOMMU_HWPT_DATA_ARM_SMMUV3`.
/// The kernel validates the STE fields and programs the host IOMMU.
#[repr(C)]
pub struct IommuHwptArmSmmuv3 {
    pub ste: [u64; 2],
}

// --- Hardware info query ---

/// HW info type: ARM SMMUv3.
pub const IOMMU_HW_INFO_TYPE_ARM_SMMUV3: u32 = 2;

#[repr(C)]
struct IommuGetHwInfo {
    size: u32,
    flags: u32,
    dev_id: u32,
    data_len: u32,
    data_uptr: u64,
    out_data_type: u32,
    out_max_pasid_log2: u8,
    __reserved: [u8; 3],
    out_capabilities: u64,
}

/// ARM SMMUv3 hardware information returned by `IOMMU_GET_HW_INFO`.
///
/// Contains the physical IOMMU's IDR register values. The VMM uses
/// these to cap the virtual SMMU's advertised capabilities.
#[repr(C)]
pub struct IommuHwInfoArmSmmuv3 {
    pub flags: u32,
    pub __reserved: u32,
    pub idr: [u32; 6],
    pub iidr: u32,
    pub aidr: u32,
}

// --- HWPT invalidation ---

/// Invalidation data type for ARM SMMUv3 (via vIOMMU).
pub const IOMMU_VIOMMU_INVALIDATE_DATA_ARM_SMMUV3: u32 = 1;

#[repr(C)]
struct IommuHwptInvalidate {
    size: u32,
    hwpt_id: u32,
    data_uptr: u64,
    data_type: u32,
    entry_len: u32,
    entry_num: u32,
    __reserved: u32,
}

/// Error from [`IommufdCtx::hwpt_invalidate`], pairing the underlying ioctl
/// errno with the kernel's reported handled-entry count.
#[derive(Debug, thiserror::Error)]
#[error("IOMMU_HWPT_INVALIDATE failed (kernel handled {handled} entries)")]
pub struct HwptInvalidateError {
    /// The underlying `IOMMU_HWPT_INVALIDATE` ioctl errno.
    #[source]
    pub errno: nix::errno::Errno,
    /// The kernel's in/out `entry_num` after the failed call: the number of
    /// leading entries it reports as handled before the failure. See the
    /// caveat on [`IommufdCtx::hwpt_invalidate`] — this is unreliable for early
    /// failures and may equal the input count.
    pub handled: u32,
}

// --- Virtual IOMMU ---

/// vIOMMU type: ARM SMMUv3.
pub const IOMMU_VIOMMU_TYPE_ARM_SMMUV3: u32 = 1;

#[repr(C)]
struct IommuViommuAlloc {
    size: u32,
    flags: u32,
    r#type: u32,
    dev_id: u32,
    hwpt_id: u32,
    out_viommu_id: u32,
    data_len: u32,
    __reserved: u32,
    data_uptr: u64,
}

// --- Virtual device ---

#[repr(C)]
struct IommuVdeviceAlloc {
    size: u32,
    viommu_id: u32,
    dev_id: u32,
    out_vdevice_id: u32,
    virt_id: u64,
}

// --- Virtual event queue ---

/// vEVENTQ type: ARM SMMUv3.
pub const IOMMU_VEVENTQ_TYPE_ARM_SMMUV3: u32 = 1;

#[repr(C)]
struct IommuVeventqAlloc {
    size: u32,
    flags: u32,
    viommu_id: u32,
    r#type: u32,
    veventq_depth: u32,
    out_veventq_id: u32,
    out_veventq_fd: u32,
    __reserved: u32,
}

/// Header for each event in a vEVENTQ fd read.
#[repr(C)]
pub struct IommufdVeventHeader {
    pub flags: u32,
    pub sequence: u32,
}

/// ARM SMMUv3 virtual event record (256-bit, little-endian).
///
/// Follows an `IommufdVeventHeader` in the vEVENTQ fd read stream.
#[repr(C)]
pub struct IommuVeventArmSmmuv3 {
    pub evt: [u64; 4],
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
        let mut flags = IOMMU_IOAS_MAP_FIXED_IOVA | IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = IommuIoasMapFile {
            size: size_of::<IommuIoasMapFile>() as u32,
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

    /// Allocate a hardware page table (HWPT).
    ///
    /// For a **nesting parent** (S2): set `flags = IOMMU_HWPT_ALLOC_NEST_PARENT`,
    /// `pt_id` = IOAS ID, `data_type = IOMMU_HWPT_DATA_NONE`.
    ///
    /// For a **nested child** (S1): set `flags = 0`, `pt_id` = parent HWPT ID
    /// or vIOMMU ID, `data_type = IOMMU_HWPT_DATA_ARM_SMMUV3`, and pass the
    /// STE data via `data`.
    ///
    /// Returns the kernel-assigned HWPT object ID.
    pub fn hwpt_alloc(
        &self,
        flags: u32,
        dev_id: u32,
        pt_id: u32,
        data_type: u32,
        data: Option<&IommuHwptArmSmmuv3>,
    ) -> anyhow::Result<u32> {
        let (data_uptr, data_len) = match data {
            Some(data) => (
                std::ptr::from_ref(data) as u64,
                size_of::<IommuHwptArmSmmuv3>() as u32,
            ),
            None => (0, 0),
        };
        let mut cmd = IommuHwptAlloc {
            size: size_of::<IommuHwptAlloc>() as u32,
            flags,
            dev_id,
            pt_id,
            out_hwpt_id: 0,
            __reserved: 0,
            data_type,
            data_len,
            data_uptr,
            fault_id: 0,
            __reserved2: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`data_len` are derived from the optional live `data`
        // borrow (or null/zero when absent), so the kernel reads only within a
        // valid, fully-initialized `#[repr(C)]` buffer.
        unsafe {
            ioctl::iommu_hwpt_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_HWPT_ALLOC failed")?;
        }
        Ok(cmd.out_hwpt_id)
    }

    /// Query hardware information for a device's IOMMU.
    ///
    /// Returns `(out_data_type, out_capabilities)`. The type-specific data is
    /// written into `out_info`.
    pub fn get_hw_info(
        &self,
        dev_id: u32,
        out_info: &mut IommuHwInfoArmSmmuv3,
    ) -> anyhow::Result<(u32, u64)> {
        let mut cmd = IommuGetHwInfo {
            size: size_of::<IommuGetHwInfo>() as u32,
            flags: 0,
            dev_id,
            data_len: size_of::<IommuHwInfoArmSmmuv3>() as u32,
            data_uptr: std::ptr::from_mut(out_info) as u64,
            out_data_type: 0,
            out_max_pasid_log2: 0,
            __reserved: [0; 3],
            out_capabilities: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`data_len` are derived from the live, exclusively
        // borrowed `out_info`, so the kernel writes at most
        // `size_of::<IommuHwInfoArmSmmuv3>()` bytes into a valid buffer. Every
        // field of that `#[repr(C)]` struct is an integer, so any bytes the
        // kernel writes form a valid value.
        unsafe {
            ioctl::iommu_get_hw_info(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_GET_HW_INFO failed")?;
        }
        Ok((cmd.out_data_type, cmd.out_capabilities))
    }

    /// Invalidate IOMMU caches via a nested HWPT or vIOMMU.
    ///
    /// `hwpt_id` is a nested HWPT ID or vIOMMU ID. Each entry in `entries` is a
    /// raw 128-bit invalidation command as a `[qw0, qw1]` quadword pair; the
    /// kernel parses the opcode and operands per `data_type`.
    ///
    /// On full success returns the number of entries handled (always
    /// `entries.len()`). On failure returns a [`HwptInvalidateError`] carrying
    /// the kernel's in/out `entry_num` — the count of leading entries it
    /// reports as handled before the failure — so the caller can locate the
    /// offending entry. The kernel writes `entry_num` back even on error.
    ///
    /// Caveat: `entry_num` is only meaningful when the kernel reached its
    /// per-entry processing loop. For an early failure (notably `-ENOMEM`
    /// allocating the kernel-side scratch array) the field is left at the
    /// input count, so [`HwptInvalidateError::handled`] can equal
    /// `entries.len()` despite nothing being handled. Callers must treat
    /// `handled >= entries.len()` on the error path as "position unknown".
    pub fn hwpt_invalidate(
        &self,
        hwpt_id: u32,
        data_type: u32,
        entries: &[[u64; 2]],
    ) -> Result<u32, HwptInvalidateError> {
        let entry_num = u32::try_from(entries.len()).map_err(|_| HwptInvalidateError {
            errno: nix::errno::Errno::EINVAL,
            handled: 0,
        })?;
        let mut cmd = IommuHwptInvalidate {
            size: size_of::<IommuHwptInvalidate>() as u32,
            hwpt_id,
            data_uptr: entries.as_ptr() as u64,
            data_type,
            entry_len: size_of::<[u64; 2]>() as u32,
            entry_num,
            __reserved: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`entry_len`/`entry_num` are derived from the live
        // `entries` slice, so the kernel reads only within a valid,
        // fully-initialized array.
        let res = unsafe { ioctl::iommu_hwpt_invalidate(self.file.as_raw_fd(), &mut cmd) };
        // The kernel writes `entry_num` (the number of entries it handled) back
        // even on failure, so read it regardless of the ioctl result.
        match res {
            Ok(_) => Ok(cmd.entry_num),
            Err(errno) => Err(HwptInvalidateError {
                errno,
                handled: cmd.entry_num,
            }),
        }
    }

    /// Allocate a virtual IOMMU (vIOMMU).
    ///
    /// `viommu_type` should be `IOMMU_VIOMMU_TYPE_ARM_SMMUV3` for SMMUv3.
    /// `dev_id` is a device bound to the physical IOMMU backing this vIOMMU.
    /// `hwpt_id` is the nesting parent HWPT to associate with.
    ///
    /// Returns the kernel-assigned vIOMMU object ID.
    pub fn viommu_alloc(&self, viommu_type: u32, dev_id: u32, hwpt_id: u32) -> anyhow::Result<u32> {
        let mut cmd = IommuViommuAlloc {
            size: size_of::<IommuViommuAlloc>() as u32,
            flags: 0,
            r#type: viommu_type,
            dev_id,
            hwpt_id,
            out_viommu_id: 0,
            data_len: 0,
            __reserved: 0,
            data_uptr: 0,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_viommu_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_VIOMMU_ALLOC failed")?;
        }
        Ok(cmd.out_viommu_id)
    }

    /// Allocate a virtual device (vDevice) on a vIOMMU.
    ///
    /// `virt_id` is the virtual stream ID (e.g., guest BDF for SMMUv3).
    ///
    /// Returns the kernel-assigned vDevice object ID.
    pub fn vdevice_alloc(&self, viommu_id: u32, dev_id: u32, virt_id: u64) -> anyhow::Result<u32> {
        let mut cmd = IommuVdeviceAlloc {
            size: size_of::<IommuVdeviceAlloc>() as u32,
            viommu_id,
            dev_id,
            out_vdevice_id: 0,
            virt_id,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_vdevice_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_VDEVICE_ALLOC failed")?;
        }
        Ok(cmd.out_vdevice_id)
    }

    /// Allocate a virtual event queue (vEVENTQ) on a vIOMMU.
    ///
    /// `veventq_type` should be `IOMMU_VEVENTQ_TYPE_ARM_SMMUV3` for SMMUv3.
    /// `depth` is the maximum number of events in the queue.
    ///
    /// Returns `(veventq_id, veventq_fd)`. The fd is an eventfd-style file
    /// descriptor that can be polled for fault events.
    pub fn veventq_alloc(
        &self,
        viommu_id: u32,
        veventq_type: u32,
        depth: u32,
    ) -> anyhow::Result<(u32, fs::File)> {
        let mut cmd = IommuVeventqAlloc {
            size: size_of::<IommuVeventqAlloc>() as u32,
            flags: 0,
            viommu_id,
            r#type: veventq_type,
            veventq_depth: depth,
            out_veventq_id: 0,
            out_veventq_fd: 0,
            __reserved: 0,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_veventq_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_VEVENTQ_ALLOC failed")?;
        }
        // SAFETY: kernel returned a valid fd in out_veventq_fd.
        let veventq_file = unsafe { fs::File::from_raw_fd(cmd.out_veventq_fd as RawFd) };
        Ok((cmd.out_veventq_id, veventq_file))
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
