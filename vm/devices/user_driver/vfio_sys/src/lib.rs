// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(unix)]
// UNSAFETY: Manual memory management with mmap and vfio ioctls.
#![expect(unsafe_code)]

use anyhow::Context;
use bitfield_struct::bitfield;
use libc::c_void;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::prelude::*;
use std::path::Path;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_ACTION_TRIGGER;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_DATA_EVENTFD;
use vfio_bindings::bindings::vfio::VFIO_IRQ_SET_DATA_NONE;
use vfio_bindings::bindings::vfio::VFIO_PCI_MSIX_IRQ_INDEX;
use vfio_bindings::bindings::vfio::vfio_device_info;
use vfio_bindings::bindings::vfio::vfio_group_status;
use vfio_bindings::bindings::vfio::vfio_irq_info;
use vfio_bindings::bindings::vfio::vfio_irq_set;
use vfio_bindings::bindings::vfio::vfio_region_info;

mod ioctl {
    use nix::request_code_none;
    use std::os::raw::c_char;
    use std::os::raw::c_int;
    use vfio_bindings::bindings::vfio::VFIO_BASE;
    use vfio_bindings::bindings::vfio::VFIO_TYPE;
    use vfio_bindings::bindings::vfio::vfio_device_info;
    use vfio_bindings::bindings::vfio::vfio_group_status;
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
    nix::ioctl_write_ptr_bad!(
        vfio_group_set_keep_alive,
        request_code_none!(VFIO_TYPE, VFIO_PRIVATE_BASE),
        c_char
    );
}

#[derive(Debug, PartialEq, Eq)]
pub enum VfioErrata {
    None,
    DelayWorkaround,
}

impl VfioErrata {
    pub fn from_error(observed: &anyhow::Error) -> Self {
        if observed.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENODEV) {
            return VfioErrata::DelayWorkaround;
        }
        return VfioErrata::None;
    }
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
}

#[repr(u32)]
pub enum IommuType {
    NoIommu = vfio_bindings::bindings::vfio::VFIO_NOIOMMU_IOMMU,
}

pub struct Group {
    file: File,
}

impl Group {
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

    pub async fn open_device(&self, device_id: &str) -> anyhow::Result<Device> {
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
    pub async fn set_keep_alive(&self, device_id: &str) -> anyhow::Result<()> {
        // SAFETY: The file descriptor is valid and a correctly constructed struct is being passed.
        unsafe {
            let id = CString::new(device_id)?;
            ioctl::vfio_group_set_keep_alive(self.file.as_raw_fd(), id.as_ptr())
                .with_context(|| format!("failed to set keep-alive for {device_id}"))
                .map(|_| ())
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
    reset: bool,
    pci: bool,
    platform: bool,
    amba: bool,
    ccw: bool,
    ap: bool,

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
        #[repr(C)]
        struct VfioIrqSetWithArray {
            header: vfio_irq_set,
            fd: [i32; 256],
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
            fd: [-1; 256],
        };

        for (x, y) in eventfd.into_iter().zip(&mut param.fd) {
            *y = x.as_fd().as_raw_fd();
            param.header.count += 1;
        }

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
