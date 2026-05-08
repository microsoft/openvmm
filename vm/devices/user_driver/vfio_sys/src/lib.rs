// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(unix)]
// UNSAFETY: Manual memory management with mmap and vfio ioctls.
#![expect(unsafe_code)]

use anyhow::Context;
use bitfield_struct::bitfield;
use headervec::HeaderVec;
use libc::c_void;
use memory_range::MemoryRange;
use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::prelude::*;
use std::path::Path;
use std::time::Duration;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_ACTION_TRIGGER;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_DATA_EVENTFD;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_DATA_NONE;
use vfio_bindings::bindings::vfio::VFIO_PCI_MSIX_IRQ_INDEX;
use vfio_bindings::bindings::vfio::VFIO_REGION_INFO_CAP_SPARSE_MMAP;
use vfio_bindings::bindings::vfio::vfio_device_info;
use vfio_bindings::bindings::vfio::vfio_group_status;
use vfio_bindings::bindings::vfio::vfio_info_cap_header;
use vfio_bindings::bindings::vfio::vfio_irq_info;
use vfio_bindings::bindings::vfio::vfio_irq_set;
use vfio_bindings::bindings::vfio::vfio_region_info;
use vfio_bindings::bindings::vfio::vfio_region_info_cap_sparse_mmap;
use vfio_bindings::bindings::vfio::vfio_region_sparse_mmap_area;

/// Returns the host page size.
pub fn host_page_size() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static PAGE_SIZE: AtomicU64 = const { AtomicU64::new(0) };

    let page_size = PAGE_SIZE.load(Relaxed);
    if page_size == 0 {
        // SAFETY: sysconf(_SC_PAGESIZE) is always safe to call on Linux.
        let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(raw > 0, "sysconf(_SC_PAGESIZE) failed: {raw}");
        let page_size = raw as u64;
        PAGE_SIZE.store(page_size, Relaxed);
        page_size
    } else {
        page_size
    }
}

mod ioctl {
    use nix::request_code_none;
    use std::os::raw::c_char;
    use std::os::raw::c_int;
    use vfio_bindings::bindings::vfio::VFIO_BASE;
    use vfio_bindings::bindings::vfio::VFIO_TYPE;
    use vfio_bindings::bindings::vfio::vfio_device_info;
    use vfio_bindings::bindings::vfio::vfio_group_status;
    use vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_map;
    use vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_unmap;
    use vfio_bindings::bindings::vfio::vfio_irq_info;
    use vfio_bindings::bindings::vfio::vfio_irq_set;
    use vfio_bindings::bindings::vfio::vfio_region_info;

    const VFIO_PRIVATE_BASE: u32 = 200;

    nix::ioctl_write_int_bad!(vfio_set_iommu, request_code_none!(VFIO_TYPE, VFIO_BASE + 2));
    nix::ioctl_read_bad!(
        vfio_group_get_status,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 3),
        vfio_group_status
    );
    nix::ioctl_write_ptr_bad!(
        vfio_group_set_container,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 4),
        c_int
    );
    nix::ioctl_write_ptr_bad!(
        vfio_group_get_device_fd,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 6),
        c_char
    );
    nix::ioctl_read_bad!(
        vfio_device_get_info,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 7),
        vfio_device_info
    );
    nix::ioctl_readwrite_bad!(
        vfio_device_get_region_info,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 8),
        vfio_region_info
    );
    nix::ioctl_readwrite_bad!(
        vfio_device_get_irq_info,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 9),
        vfio_irq_info
    );
    nix::ioctl_write_ptr_bad!(
        vfio_device_set_irqs,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 10),
        vfio_irq_set
    );
    nix::ioctl_none_bad!(
        vfio_device_reset,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 11)
    );
    nix::ioctl_write_ptr_bad!(
        vfio_group_set_keep_alive,
        request_code_none!(VFIO_TYPE, VFIO_PRIVATE_BASE),
        c_char
    );
    // VFIO_IOMMU_MAP_DMA
    nix::ioctl_write_ptr_bad!(
        vfio_iommu_map_dma,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 13),
        vfio_iommu_type1_dma_map
    );
    // VFIO_IOMMU_UNMAP_DMA
    nix::ioctl_readwrite_bad!(
        vfio_iommu_unmap_dma,
        request_code_none!(VFIO_TYPE, VFIO_BASE + 14),
        vfio_iommu_type1_dma_unmap
    );
}

pub struct Container {
    file: File,
}

impl Container {
    pub fn new() -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vfio/vfio")
            .context("failed to open /dev/vfio/vfio")?;

        Ok(Self { file })
    }

    pub fn set_iommu(&self, iommu: IommuType) -> anyhow::Result<()> {
        // SAFETY: The file descriptor is valid.
        unsafe {
            ioctl::vfio_set_iommu(self.file.as_raw_fd(), iommu as i32)
                .context("failed to set iommu")?;
        }
        Ok(())
    }

    /// Map a host virtual address range into the IOMMU for device DMA access.
    ///
    /// `iova` is the IO virtual address the device will use (typically the
    /// guest physical address). `vaddr` is the host virtual address backing
    /// the memory. `size` is the length in bytes. All three must be
    /// page-aligned.
    ///
    /// Only valid when the container uses a Type1v2 IOMMU.
    ///
    /// # Safety
    /// `vaddr` must point to valid, backed memory for `size` bytes. The
    /// memory must not be unmapped while the IOMMU mapping is live (until
    /// a corresponding `unmap_dma` call).
    pub unsafe fn map_dma(&self, iova: u64, vaddr: *const u8, size: u64) -> anyhow::Result<()> {
        use vfio_bindings::bindings::vfio::VFIO_DMA_MAP_FLAG_READ;
        use vfio_bindings::bindings::vfio::VFIO_DMA_MAP_FLAG_WRITE;

        let page_size = host_page_size();
        let page_mask = page_size - 1;
        let vaddr = vaddr as u64;
        anyhow::ensure!(
            iova & page_mask == 0 && vaddr & page_mask == 0 && size & page_mask == 0,
            "VFIO DMA mapping requires page-aligned iova ({iova:#x}), vaddr ({vaddr:#x}), and size ({size:#x}), page size {page_size:#x}"
        );

        let dma_map = vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_map {
            argsz: size_of::<vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_map>() as u32,
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            vaddr,
            iova,
            size,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed
        // struct is being passed.
        unsafe {
            ioctl::vfio_iommu_map_dma(self.file.as_raw_fd(), &dma_map)
                .context("VFIO_IOMMU_MAP_DMA failed")?;
        }
        Ok(())
    }

    /// Unmap a previously mapped IOVA range from the IOMMU.
    ///
    /// For Type1v2, the unmap range must not bisect any previous mapping:
    /// if a mapping exists at `iova`, it must start exactly at `iova`, and
    /// if a mapping exists at `iova + size - 1`, it must end there.
    /// Multiple mappings may be unmapped in one call as long as these
    /// boundary conditions hold. Gaps within the range are fine.
    pub fn unmap_dma(&self, iova: u64, size: u64) -> anyhow::Result<()> {
        let mut dma_unmap = vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_unmap {
            argsz: size_of::<vfio_bindings::bindings::vfio::vfio_iommu_type1_dma_unmap>() as u32,
            flags: 0,
            iova,
            size,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed
        // struct is being passed.
        unsafe {
            ioctl::vfio_iommu_unmap_dma(self.file.as_raw_fd(), &mut dma_unmap)
                .context("VFIO_IOMMU_UNMAP_DMA failed")?;
        }
        if dma_unmap.size != size {
            tracing::warn!(
                iova,
                requested = size,
                actual = dma_unmap.size,
                "VFIO_IOMMU_UNMAP_DMA: unmapped size differs from requested"
            );
        }
        Ok(())
    }
}

/// IOMMU type for VFIO container.
///
/// Only Type1v2 and NoIommu are supported. Type1 (v1) is a legacy interface
/// that does not support fine-grained DMA mapping and is intentionally excluded.
#[repr(u32)]
pub enum IommuType {
    Type1v2 = vfio_bindings::bindings::vfio::VFIO_TYPE1v2_IOMMU,
    NoIommu = vfio_bindings::bindings::vfio::VFIO_NOIOMMU_IOMMU,
}

pub struct Group {
    file: File,
}

impl Group {
    /// Construct a `Group` from a pre-opened VFIO group file descriptor.
    pub fn from_file(file: File) -> Self {
        Self { file }
    }

    pub fn open(group: u64) -> anyhow::Result<Self> {
        Self::open_path(format!("/dev/vfio/{group}").as_ref())
    }

    pub fn open_noiommu(group: u64) -> anyhow::Result<Self> {
        Self::open_path(format!("/dev/vfio/noiommu-{group}").as_ref())
    }

    fn open_path(group: &Path) -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(group)
            .with_context(|| format!("failed to open group {}", group.display()))?;

        Ok(Self { file })
    }

    pub fn find_group_for_device(device_sysfs_path: &Path) -> anyhow::Result<u64> {
        let group = device_sysfs_path.join("iommu_group");
        let group = fs::read_link(group).context("failed to read iommu group")?;
        let group: u64 = group
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid group link")?
            .parse()
            .context("failed to parse iommu group")?;

        Ok(group)
    }

    pub fn open_device(&self, device_id: &str) -> anyhow::Result<Device> {
        let id = CString::new(device_id)?;
        // SAFETY: The file descriptor is valid and the string is null-terminated.
        let file = unsafe {
            let fd = ioctl::vfio_group_get_device_fd(self.file.as_raw_fd(), id.as_ptr())
                .with_context(|| format!("failed to get device fd for {device_id}"))?;
            File::from_raw_fd(fd)
        };

        Ok(Device { file })
    }

    pub fn set_container(&self, container: &Container) -> anyhow::Result<()> {
        // SAFETY: The file descriptors are valid.
        unsafe {
            ioctl::vfio_group_set_container(self.file.as_raw_fd(), &container.file.as_raw_fd())
                .context("failed to set container")?;
        }
        Ok(())
    }

    /// Try to attach this group to the given container.
    ///
    /// Returns `Ok(true)` if the group was successfully attached, `Ok(false)`
    /// if the kernel rejected the pairing (EINVAL — the IOMMU domains are
    /// incompatible), or `Err` on unexpected failures.
    pub fn try_set_container(&self, container: &Container) -> anyhow::Result<bool> {
        // SAFETY: The file descriptors are valid.
        let result = unsafe {
            ioctl::vfio_group_set_container(self.file.as_raw_fd(), &container.file.as_raw_fd())
        };
        match result {
            Ok(_) => Ok(true),
            Err(nix::errno::Errno::EINVAL) => Ok(false),
            Err(e) => Err(e).context("failed to set container"),
        }
    }

    pub fn status(&self) -> anyhow::Result<GroupStatus> {
        let mut status = vfio_group_status {
            argsz: size_of::<vfio_group_status>() as u32,
            flags: 0,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_group_get_status(self.file.as_raw_fd(), &mut status)
                .context("failed to get group status")?;
        };
        Ok(GroupStatus::from(status.flags))
    }

    /// Skip VFIO device reset when kernel is reloaded during servicing.
    /// This feature is non-upstream version of our kernel and will be
    /// eventually replaced with iommufd.
    pub fn set_keep_alive(&self, device_id: &str) -> anyhow::Result<()> {
        let id = CString::new(device_id)?;
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_group_set_keep_alive(self.file.as_raw_fd(), id.as_ptr())
                .with_context(|| format!("failed to set keep-alive for {device_id}"))?;
        }
        Ok(())
    }
}

/// Retry wrapper for VFIO operations that may transiently fail
pub struct VfioRetry<'a> {
    driver: &'a dyn Driver,
    device_id: &'a str,
    sleep_duration: Duration,
    max_retries: u32,
}

impl<'a> VfioRetry<'a> {
    const SLEEP_DURATION: Duration = Duration::from_millis(250);
    const MAX_RETRIES: u32 = 1;

    pub fn new(driver: &'a dyn Driver, device_id: &'a str) -> Self {
        Self {
            driver,
            device_id,
            sleep_duration: Self::SLEEP_DURATION,
            max_retries: Self::MAX_RETRIES,
        }
    }

    /// Retry `op` when `should_retry` returns true for the error, up to
    /// `max_retries` times with a sleep between attempts.
    pub async fn retry<T, E>(
        &self,
        mut op: impl FnMut() -> Result<T, E>,
        should_retry: impl Fn(&E) -> bool,
        context: &str,
    ) -> Result<T, E>
    where
        E: std::fmt::Display,
    {
        let mut attempt = 0;
        loop {
            match op() {
                Ok(val) => return Ok(val),
                Err(err) => {
                    if attempt >= self.max_retries || !should_retry(&err) {
                        return Err(err);
                    }
                    attempt += 1;
                    tracelimit::warn_ratelimited!(
                        device_id = self.device_id,
                        operation = context,
                        attempt,
                        "retrying after transient error: {err}"
                    );
                }
            }
            PolledTimer::new(self.driver)
                .sleep(self.sleep_duration)
                .await;
        }
    }
}

#[bitfield(u32)]
pub struct GroupStatus {
    pub viable: bool,
    pub container_set: bool,

    #[bits(30)]
    _reserved: u32,
}

pub struct Device {
    file: File,
}

#[derive(Debug)]
pub struct DeviceInfo {
    pub flags: DeviceFlags,
    pub num_regions: u32,
    pub num_irqs: u32,
}

#[bitfield(u32)]
pub struct DeviceFlags {
    pub reset: bool,
    pub pci: bool,
    pub platform: bool,
    pub amba: bool,
    pub ccw: bool,
    pub ap: bool,

    #[bits(26)]
    _reserved: u32,
}

#[derive(Debug)]
pub struct RegionInfo {
    pub flags: RegionFlags,
    pub size: u64,
    pub offset: u64,
}

#[bitfield(u32)]
pub struct RegionFlags {
    read: bool,
    write: bool,
    mmap: bool,
    caps: bool,

    #[bits(28)]
    _reserved: u32,
}

#[derive(Debug)]
pub struct IrqInfo {
    pub flags: IrqFlags,
    pub count: u32,
}

#[bitfield(u32)]
pub struct IrqFlags {
    eventfd: bool,
    maskable: bool,
    automasked: bool,
    pub noresize: bool,

    #[bits(28)]
    _reserved: u32,
}

impl Device {
    pub fn info(&self) -> anyhow::Result<DeviceInfo> {
        let mut info = vfio_device_info {
            argsz: size_of::<vfio_device_info>() as u32,
            flags: 0,
            num_regions: 0,
            num_irqs: 0,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_device_get_info(self.file.as_raw_fd(), &mut info)
                .context("failed to get device info")?;
        }
        Ok(DeviceInfo {
            flags: DeviceFlags::from(info.flags),
            num_regions: info.num_regions,
            num_irqs: info.num_irqs,
        })
    }

    pub fn region_info(&self, index: u32) -> anyhow::Result<RegionInfo> {
        let mut info = vfio_region_info {
            argsz: size_of::<vfio_region_info>() as u32,
            index,
            flags: 0,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_device_get_region_info(self.file.as_raw_fd(), &mut info)
                .context("failed to get region info")?;
        };
        Ok(RegionInfo {
            flags: RegionFlags::from(info.flags),
            size: info.size,
            offset: info.offset,
        })
    }

    /// Query the mmappable sub-regions for a VFIO region.
    ///
    /// If the region has a `VFIO_REGION_INFO_CAP_SPARSE_MMAP` capability,
    /// returns the list of mmappable areas from it. If the region supports
    /// mmap but has no sparse capability, returns a single area covering
    /// the entire region. Returns an empty list if the region does not
    /// support mmap.
    pub fn region_mmap_areas(&self, index: u32) -> anyhow::Result<Vec<MemoryRange>> {
        let mut info = vfio_region_info {
            argsz: size_of::<vfio_region_info>() as u32,
            index,
            flags: 0,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_device_get_region_info(self.file.as_raw_fd(), &mut info)
                .context("failed to get region info")?;
        };

        let flags = RegionFlags::from(info.flags);

        // If the kernel indicates capabilities are present and returned a
        // larger argsz, re-query with a sufficiently large buffer to
        // retrieve the capability chain.
        if flags.caps() && info.argsz > size_of::<vfio_region_info>() as u32 {
            let buf_size = info.argsz as usize;
            let tail_len = buf_size - size_of::<vfio_region_info>();
            let mut buf = HeaderVec::<vfio_region_info, u8, 0>::with_capacity(
                vfio_region_info {
                    argsz: buf_size as u32,
                    index,
                    flags: 0,
                    cap_offset: 0,
                    size: 0,
                    offset: 0,
                },
                tail_len,
            );
            // SAFETY: The buffer is properly aligned and large enough per the
            // kernel's argsz, and the fd is valid.
            unsafe {
                ioctl::vfio_device_get_region_info(self.file.as_raw_fd(), buf.as_mut_ptr())
                    .context("failed to get region info with capabilities")?;
            }
            // Use the kernel's returned argsz rather than our pre-computed
            // value, in case it differs.
            let actual_tail = buf.head.argsz as usize - size_of::<vfio_region_info>();
            // SAFETY: The kernel initialized the tail bytes via the ioctl.
            unsafe { buf.set_tail_len(actual_tail.min(tail_len)) };
            if let Some(areas) = parse_sparse_mmap_caps(&buf) {
                return Ok(areas);
            }
        }

        if flags.mmap() {
            Ok(vec![MemoryRange::new(0..info.size)])
        } else {
            Ok(Vec::new())
        }
    }

    pub fn irq_info(&self, index: u32) -> anyhow::Result<IrqInfo> {
        let mut info = vfio_irq_info {
            argsz: size_of::<vfio_irq_info>() as u32,
            index,
            flags: 0,
            count: 0,
        };
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_device_get_irq_info(self.file.as_raw_fd(), &mut info)
                .context("failed to get irq info")?;
        }
        Ok(IrqInfo {
            flags: IrqFlags::from(info.flags),
            count: info.count,
        })
    }

    pub fn map(&self, offset: u64, len: usize, write: bool) -> anyhow::Result<MappedRegion> {
        let mut prot = libc::PROT_READ;
        if write {
            prot |= libc::PROT_WRITE;
        }
        // SAFETY: The file descriptor is valid and no address is being passed.
        // The result is being validated.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                offset as i64,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("failed to map region");
        }
        Ok(MappedRegion { addr, len })
    }

    pub fn map_msix<I>(&self, start: u32, eventfd: I) -> anyhow::Result<()>
    where
        I: IntoIterator,
        I::Item: AsFd,
    {
        const MAX_MSIX_VECTORS: usize = 256;

        #[repr(C)]
        struct VfioIrqSetWithArray {
            header: vfio_irq_set,
            fd: [i32; MAX_MSIX_VECTORS],
        }
        let mut param = VfioIrqSetWithArray {
            header: vfio_irq_set {
                argsz: size_of::<VfioIrqSetWithArray>() as u32,
                flags: VFIO_IRQ_SET_ACTION_TRIGGER,
                index: VFIO_PCI_MSIX_IRQ_INDEX,
                start,
                count: 0,
                // data is a zero-sized array, the real data is fd.
                data: Default::default(),
            },
            fd: [-1; MAX_MSIX_VECTORS],
        };

        let fds: Vec<_> = eventfd.into_iter().collect();
        anyhow::ensure!(
            fds.len() <= MAX_MSIX_VECTORS,
            "MSI-X vector count {} exceeds maximum {MAX_MSIX_VECTORS}",
            fds.len()
        );

        let mut count = 0u32;
        for (x, y) in fds.iter().zip(&mut param.fd) {
            *y = x.as_fd().as_raw_fd();
            count += 1;
        }
        param.header.count = count;

        if param.header.count == 0 {
            param.header.flags |= VFIO_IRQ_SET_DATA_NONE;
        } else {
            param.header.flags |= VFIO_IRQ_SET_DATA_EVENTFD;
        }

        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            ioctl::vfio_device_set_irqs(self.file.as_raw_fd(), &param.header)
                .context("failed to set msi-x trigger")?;
        }
        Ok(())
    }

    /// Disable (unmap) a contiguous range of previously mapped MSI-X vectors.
    ///
    /// This issues VFIO_DEVICE_SET_IRQS with ACTION_TRIGGER + DATA_NONE and a
    /// non-zero count, which per VFIO semantics removes the eventfd bindings
    /// for the specified range starting at `start`.
    pub fn unmap_msix(&self, start: u32, count: u32) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        let header = vfio_irq_set {
            argsz: size_of::<vfio_irq_set>() as u32,
            flags: VFIO_IRQ_SET_ACTION_TRIGGER | VFIO_IRQ_SET_DATA_NONE,
            index: VFIO_PCI_MSIX_IRQ_INDEX,
            start,
            count,
            data: Default::default(),
        };

        // SAFETY: The file descriptor is valid; header constructed per VFIO spec.
        unsafe {
            ioctl::vfio_device_set_irqs(self.file.as_raw_fd(), &header)
                .context("failed to unmap msix vectors")?;
        }
        Ok(())
    }

    /// Reset the device via VFIO_DEVICE_RESET.
    ///
    /// Not all devices support reset — check `DeviceInfo::flags.reset()`
    /// first. Returns an error if the ioctl fails.
    pub fn reset(&self) -> anyhow::Result<()> {
        // SAFETY: The file descriptor is valid.
        unsafe {
            ioctl::vfio_device_reset(self.file.as_raw_fd()).context("VFIO_DEVICE_RESET failed")?;
        }
        Ok(())
    }
}

/// Walk the VFIO capability chain in a region info buffer and extract sparse
/// mmap areas from any `VFIO_REGION_INFO_CAP_SPARSE_MMAP` capability.
///
/// Returns `Some(areas)` if the sparse mmap capability is present (even if
/// empty), or `None` if it is absent.
fn parse_sparse_mmap_caps(buf: &HeaderVec<vfio_region_info, u8, 0>) -> Option<Vec<MemoryRange>> {
    let mut offset = buf.head.cap_offset as usize;

    // SAFETY: HeaderVec guarantees head + tail are contiguous.
    let bytes =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.total_byte_len()) };

    while offset != 0 {
        if offset + size_of::<vfio_info_cap_header>() > bytes.len() {
            tracing::warn!(offset, "VFIO cap header extends beyond buffer");
            break;
        }

        // SAFETY: Bounds checked above. The kernel places capabilities at
        // aligned offsets within the buffer.
        let header = unsafe { &*bytes.as_ptr().add(offset).cast::<vfio_info_cap_header>() };

        if header.id as u32 == VFIO_REGION_INFO_CAP_SPARSE_MMAP {
            if offset + size_of::<vfio_region_info_cap_sparse_mmap>() > bytes.len() {
                tracing::warn!("VFIO sparse mmap cap truncated");
                break;
            }
            // SAFETY: Bounds checked above; repr(C) struct at kernel-aligned offset.
            let cap = unsafe {
                &*bytes
                    .as_ptr()
                    .add(offset)
                    .cast::<vfio_region_info_cap_sparse_mmap>()
            };
            let n = cap.nr_areas as usize;
            let areas_end = offset
                + size_of::<vfio_region_info_cap_sparse_mmap>()
                + n * size_of::<vfio_region_sparse_mmap_area>();
            if areas_end > bytes.len() {
                tracing::warn!(n, "VFIO sparse mmap areas extend beyond buffer");
                break;
            }
            // SAFETY: Bounds checked; flexible array immediately follows the fixed fields.
            let areas = unsafe { cap.areas.as_slice(n) };
            return Some(
                areas
                    .iter()
                    .filter(|a| a.size > 0)
                    .map(|a| MemoryRange::new(a.offset..a.offset + a.size))
                    .collect(),
            );
        }

        offset = header.next as usize;
    }

    // No sparse mmap cap found.
    None
}

impl AsRef<File> for Device {
    fn as_ref(&self) -> &File {
        &self.file
    }
}

impl AsFd for Device {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

/// Find the Linux irq number for the MSI-X `index` of the PCI device `pci_id`.
pub fn find_msix_irq(pci_id: &str, index: u32) -> anyhow::Result<u32> {
    let buffered = BufReader::new(File::open("/proc/interrupts")?);

    let id = format!("vfio-msix[{}]({})", index, pci_id);
    let match_str = buffered
        .lines()
        .map_while(Result::ok)
        .find(|line| line.contains(&id))
        .with_context(|| format!("cannot find interrupt {id} in /proc/interrupts"))?;

    // irq format is: <irq#:> cpu# <irq name>
    let irq = match_str.trim_start().split(':').next().unwrap();
    let irq: u32 = irq
        .parse()
        .with_context(|| format!("unexpected irq format {}. Expecting 'irq#:'", irq))?;

    Ok(irq)
}

pub fn print_relevant_params() {
    #[derive(Debug)]
    struct Param {
        _name: &'static str,
        _value: Option<String>,
    }

    let vfio_params = [
        "/sys/module/vfio/parameters/enable_unsafe_noiommu_mode",
        "/sys/module/driver/parameters/async_probe",
    ]
    .iter()
    .map(|path| Param {
        _name: path,
        _value: fs::read_to_string(path).ok().map(|s| s.trim().to_string()),
    })
    .collect::<Vec<_>>();

    tracing::debug!(
        vfio_params = ?vfio_params,
        "Relevant VFIO module parameters"
    );
}

pub struct MappedRegion {
    addr: *mut c_void,
    len: usize,
}

// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Send for MappedRegion {}
// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Sync for MappedRegion {}

impl MappedRegion {
    pub fn as_ptr(&self) -> *mut c_void {
        self.addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn read_u32(&self, offset: usize) -> u32 {
        assert_eq!(offset % 4, 0);
        assert!(offset.saturating_add(4) <= self.len);
        // SAFETY: We have validated that the offset is inside the region.
        unsafe { std::ptr::read_volatile(self.addr.byte_add(offset).cast()) }
    }

    pub fn read_u64(&self, offset: usize) -> u64 {
        assert_eq!(offset % 8, 0);
        assert!(offset.saturating_add(8) <= self.len);
        // SAFETY: We have validated that the offset is inside the region.
        unsafe { std::ptr::read_volatile(self.addr.byte_add(offset).cast()) }
    }

    pub fn write_u32(&self, offset: usize, data: u32) {
        assert_eq!(offset % 4, 0);
        assert!(offset.saturating_add(4) <= self.len);
        // SAFETY: We have validated that the offset is inside the region.
        unsafe {
            std::ptr::write_volatile(self.addr.byte_add(offset).cast(), data);
        }
    }

    pub fn write_u64(&self, offset: usize, data: u64) {
        assert_eq!(offset % 8, 0);
        assert!(offset.saturating_add(8) <= self.len);
        // SAFETY: We have validated that the offset is inside the region.
        unsafe {
            std::ptr::write_volatile(self.addr.byte_add(offset).cast(), data);
        }
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // SAFETY: The address and length are a valid mmap result.
        unsafe {
            libc::munmap(self.addr, self.len);
        }
    }
}
