// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHD and VHDX disk implementation using the Windows VHDMP driver.

#![cfg(windows)]
// UNSAFETY: Calling Win32 VirtualDisk APIs and accessing the unions they return.
#![expect(unsafe_code)]

use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_file::FileDisk;
use guid::Guid;
use inspect::Inspect;
use mesh::MeshPayload;
use scsi_buffers::RequestBuffers;
use std::fs;
use std::os::windows::prelude::*;
use std::path::Path;
use thiserror::Error;
use vm_resource::ResolveResource;
use vm_resource::ResourceId;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskHandleKind;

mod virtdisk {
    #![expect(non_snake_case, dead_code, clippy::upper_case_acronyms)]

    use guid::Guid;
    use std::os::windows::prelude::*;
    use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::core::GUID;

    // Local type aliases matching Win32 typedefs
    type BOOL = i32; // BOOL
    type ULONG = u32; // unsigned long
    type ULONGLONG = u64; // unsigned long long
    type PCWSTR = *const u16; // const WCHAR*

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    pub struct VIRTUAL_STORAGE_TYPE {
        pub DeviceId: u32,
        pub VendorId: GUID,
    }

    pub const VIRTUAL_STORAGE_TYPE_DEVICE_UNKNOWN: u32 = 0;
    pub const VIRTUAL_STORAGE_TYPE_DEVICE_ISO: u32 = 1;
    pub const VIRTUAL_STORAGE_TYPE_DEVICE_VHD: u32 = 2;
    pub const VIRTUAL_STORAGE_TYPE_DEVICE_VHDX: u32 = 3;

    pub const VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT: Guid =
        guid::guid!("EC984AEC-A0F9-47e9-901F-71415A66345B");

    // Open the backing store without opening any differencing chain parents.
    // This allows one to fixup broken parent links.
    pub const OPEN_VIRTUAL_DISK_FLAG_NO_PARENTS: u32 = 0x0000_0001;

    // The backing store being opened is an empty file. Do not perform virtual
    // disk verification.
    pub const OPEN_VIRTUAL_DISK_FLAG_BLANK_FILE: u32 = 0x0000_0002;

    // This flag is only specified at boot time to load the system disk
    // during virtual disk boot.  Must be kernel mode to specify this flag.
    pub const OPEN_VIRTUAL_DISK_FLAG_BOOT_DRIVE: u32 = 0x0000_0004;

    // This flag causes the backing file to be opened in cached mode.
    pub const OPEN_VIRTUAL_DISK_FLAG_CACHED_IO: u32 = 0x0000_0008;

    // Open the backing store without opening any differencing chain parents.
    // This allows one to fixup broken parent links temporarily without updating
    // the parent locator.
    pub const OPEN_VIRTUAL_DISK_FLAG_CUSTOM_DIFF_CHAIN: u32 = 0x0000_0010;

    // This flag causes all backing stores except the leaf backing store to
    // be opened in cached mode.
    pub const OPEN_VIRTUAL_DISK_FLAG_PARENT_CACHED_IO: u32 = 0x0000_0020;

    // This flag causes a Vhd Set file to be opened without any virtual disk.
    pub const OPEN_VIRTUAL_DISK_FLAG_VHDSET_FILE_ONLY: u32 = 0x0000_0040;

    // For differencing disks, relative parent locators are not used when
    // determining the path of a parent VHD.
    pub const OPEN_VIRTUAL_DISK_FLAG_IGNORE_RELATIVE_PARENT_LOCATOR: u32 = 0x0000_0080;

    // Disable flushing and FUA (both for payload data and for metadata)
    // for backing files associated with this virtual disk.
    pub const OPEN_VIRTUAL_DISK_FLAG_NO_WRITE_HARDENING: u32 = 0x0000_0100;

    #[repr(C)]
    pub struct OPEN_VIRTUAL_DISK_PARAMETERS {
        pub Version: u32,
        pub u: OPEN_VIRTUAL_DISK_PARAMETERS_u,
    }

    #[repr(C)]
    pub union OPEN_VIRTUAL_DISK_PARAMETERS_u {
        pub Version1: OPEN_VIRTUAL_DISK_PARAMETERS_1,
        pub Version2: OPEN_VIRTUAL_DISK_PARAMETERS_2,
        pub Version3: OPEN_VIRTUAL_DISK_PARAMETERS_3,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct OPEN_VIRTUAL_DISK_PARAMETERS_1 {
        pub RWDepth: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct OPEN_VIRTUAL_DISK_PARAMETERS_2 {
        pub GetInfoOnly: BOOL,
        pub ReadOnly: BOOL,
        pub ResiliencyGuid: GUID,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct OPEN_VIRTUAL_DISK_PARAMETERS_3 {
        pub GetInfoOnly: BOOL,
        pub ReadOnly: BOOL,
        pub ResiliencyGuid: GUID,
        pub SnapshotId: GUID,
    }

    // Pre-allocate all physical space necessary for the virtual
    // size of the disk (e.g. a fixed VHD).
    pub const CREATE_VIRTUAL_DISK_FLAG_FULL_PHYSICAL_ALLOCATION: u32 = 0x1;

    // Take ownership of the source disk during create from source disk, to
    // insure the source disk does not change during the create operation.  The
    // source disk must also already be offline or read-only (or both).
    // Ownership is released when create is done.  This also has a side-effect
    // of disallowing concurrent create from same source disk.  Create will fail
    // if ownership cannot be obtained or if the source disk is not already
    // offline or read-only.  This flag is optional, but highly recommended for
    // creates from source disk.  No effect for other types of create (no effect
    // for create from source VHD; no effect for create without SourcePath).
    pub const CREATE_VIRTUAL_DISK_FLAG_PREVENT_WRITES_TO_SOURCE_DISK: u32 = 0x2;

    // Do not copy initial virtual disk metadata or block states from the
    // parent VHD; this is useful if the parent VHD is a stand-in file and the
    // real parent will be explicitly set later.
    pub const CREATE_VIRTUAL_DISK_FLAG_DO_NOT_COPY_METADATA_FROM_PARENT: u32 = 0x4;

    // Create the backing storage disk.
    pub const CREATE_VIRTUAL_DISK_FLAG_CREATE_BACKING_STORAGE: u32 = 0x8;

    // If set, the SourceLimitPath is an change tracking ID, and all data that has changed
    // since that change tracking ID will be copied from the source. If clear, the
    // SourceLimitPath is a VHD file path in the source VHD's chain, and
    // all data that is present in the children of that VHD in the chain
    // will be copied from the source.
    pub const CREATE_VIRTUAL_DISK_FLAG_USE_CHANGE_TRACKING_SOURCE_LIMIT: u32 = 0x10;

    // If set and the parent VHD has change tracking enabled, the child will
    // have change tracking enabled and will recognize all change tracking
    // IDs that currently exist in the parent. If clear or if the parent VHD
    // does not have change tracking available, then change tracking will
    // not be enabled in the new VHD.
    pub const CREATE_VIRTUAL_DISK_FLAG_PRESERVE_PARENT_CHANGE_TRACKING_STATE: u32 = 0x20;

    // When creating a VHD Set from source, don't copy the data in the original
    // backing store, but intsead use the file as is. If this flag is not specified
    // and a source file is passed to CreateVirtualDisk for a VHDSet file, the data
    // in the source file is copied. If this flag is set the data is moved. The
    // name of the file may change.
    pub const CREATE_VIRTUAL_DISK_FLAG_VHD_SET_USE_ORIGINAL_BACKING_STORAGE: u32 = 0x40;

    // When creating a fixed virtual disk, take advantage of an underlying sparse file.
    // Only supported on file systems that support sparse VDLs.
    pub const CREATE_VIRTUAL_DISK_FLAG_SPARSE_FILE: u32 = 0x80;

    // Creates a VHD suitable as the backing store for a virtual persistent memory device.
    pub const CREATE_VIRTUAL_DISK_FLAG_PMEM_COMPATIBLE: u32 = 0x100;

    // Allow a VHD to be created on a compressed volume.
    pub const CREATE_VIRTUAL_DISK_FLAG_SUPPORT_COMPRESSED_VOLUMES: u32 = 0x200;

    // Allow a VHD to be created when it may be marked as a sparse file. This flag is a companion
    // to CREATE_VIRTUAL_DISK_FLAG_SPARSE_FILE, and overrides the behavior that only
    // allows sparse files on file systems that support sparse VDLs.
    pub const CREATE_VIRTUAL_DISK_FLAG_SUPPORT_SPARSE_FILES_ANY_FS: u32 = 0x400;

    #[repr(C)]
    pub struct CREATE_VIRTUAL_DISK_PARAMETERS {
        pub Version: u32,
        pub u: CREATE_VIRTUAL_DISK_PARAMETERS_u,
    }

    #[repr(C)]
    pub union CREATE_VIRTUAL_DISK_PARAMETERS_u {
        pub Version1: CREATE_VIRTUAL_DISK_PARAMETERS_1,
        pub Version2: CREATE_VIRTUAL_DISK_PARAMETERS_2,
        pub Version3: CREATE_VIRTUAL_DISK_PARAMETERS_3,
        pub Version4: CREATE_VIRTUAL_DISK_PARAMETERS_4,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct CREATE_VIRTUAL_DISK_PARAMETERS_1 {
        pub UniqueId: GUID,
        pub MaximumSize: ULONGLONG,
        pub BlockSizeInBytes: ULONG,
        pub SectorSizeInBytes: ULONG,
        pub ParentPath: PCWSTR,
        pub SourcePath: PCWSTR,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct CREATE_VIRTUAL_DISK_PARAMETERS_2 {
        pub UniqueId: GUID,
        pub MaximumSize: ULONGLONG,
        pub BlockSizeInBytes: ULONG,
        pub SectorSizeInBytes: ULONG,
        pub PhysicalSectorSizeInBytes: ULONG,
        pub ParentPath: PCWSTR,
        pub SourcePath: PCWSTR,
        pub OpenFlags: u32,
        pub ParentVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub SourceVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub ResiliencyGuid: GUID,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct CREATE_VIRTUAL_DISK_PARAMETERS_3 {
        pub UniqueId: GUID,
        pub MaximumSize: ULONGLONG,
        pub BlockSizeInBytes: ULONG,
        pub SectorSizeInBytes: ULONG,
        pub PhysicalSectorSizeInBytes: ULONG,
        pub ParentPath: PCWSTR,
        pub SourcePath: PCWSTR,
        pub OpenFlags: u32,
        pub ParentVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub SourceVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub ResiliencyGuid: GUID,
        pub SourceLimitPath: PCWSTR,
        pub BackingStorageType: VIRTUAL_STORAGE_TYPE,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct CREATE_VIRTUAL_DISK_PARAMETERS_4 {
        pub UniqueId: GUID,
        pub MaximumSize: ULONGLONG,
        pub BlockSizeInBytes: ULONG,
        pub SectorSizeInBytes: ULONG,
        pub PhysicalSectorSizeInBytes: ULONG,
        pub ParentPath: PCWSTR,
        pub SourcePath: PCWSTR,
        pub OpenFlags: u32,
        pub ParentVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub SourceVirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub ResiliencyGuid: GUID,
        pub SourceLimitPath: PCWSTR,
        pub BackingStorageType: VIRTUAL_STORAGE_TYPE,
        pub PmemAddressAbstractionType: GUID,
        pub DataAlignment: ULONGLONG,
    }

    pub const VIRTUAL_DISK_ACCESS_ATTACH_RO: u32 = 0x00010000;
    pub const VIRTUAL_DISK_ACCESS_ATTACH_RW: u32 = 0x00020000;
    pub const VIRTUAL_DISK_ACCESS_DETACH: u32 = 0x00040000;
    pub const VIRTUAL_DISK_ACCESS_GET_INFO: u32 = 0x00080000;
    pub const VIRTUAL_DISK_ACCESS_CREATE: u32 = 0x00100000;
    pub const VIRTUAL_DISK_ACCESS_METAOPS: u32 = 0x00200000;
    pub const VIRTUAL_DISK_ACCESS_READ: u32 = 0x000d0000;

    // Attach the disk as read only
    pub const ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY: u32 = 0x0000_0001;

    // Will cause all volumes on the disk to be mounted
    // without drive letters.
    pub const ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER: u32 = 0x0000_0002;

    // Will decouple the disk lifetime from that of the VirtualDiskHandle.
    // The disk will be attached until an explicit call is made to
    // DetachVirtualDisk, even if all handles are closed.
    pub const ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME: u32 = 0x0000_0004;

    // Indicates that the drive will not be attached to
    // the local system (but rather to a VM).
    pub const ATTACH_VIRTUAL_DISK_FLAG_NO_LOCAL_HOST: u32 = 0x0000_0008;

    // Do not assign a custom security descriptor to the disk; use the
    // system default.
    pub const ATTACH_VIRTUAL_DISK_FLAG_NO_SECURITY_DESCRIPTOR: u32 = 0x0000_0010;

    // Default volume encryption policies should not be applied to the
    // disk when attached to the local system.
    pub const ATTACH_VIRTUAL_DISK_FLAG_BYPASS_DEFAULT_ENCRYPTION_POLICY: u32 = 0x0000_0020;

    pub const GET_VIRTUAL_DISK_INFO_UNSPECIFIED: u32 = 0;
    pub const GET_VIRTUAL_DISK_INFO_SIZE: u32 = 1;
    pub const GET_VIRTUAL_DISK_INFO_IDENTIFIER: u32 = 2;
    pub const GET_VIRTUAL_DISK_INFO_PARENT_LOCATION: u32 = 3;
    pub const GET_VIRTUAL_DISK_INFO_PARENT_IDENTIFIER: u32 = 4;
    pub const GET_VIRTUAL_DISK_INFO_PARENT_TIMESTAMP: u32 = 5;
    pub const GET_VIRTUAL_DISK_INFO_VIRTUAL_STORAGE_TYPE: u32 = 6;
    pub const GET_VIRTUAL_DISK_INFO_PROVIDER_SUBTYPE: u32 = 7;
    pub const GET_VIRTUAL_DISK_INFO_IS_4K_ALIGNED: u32 = 8;
    pub const GET_VIRTUAL_DISK_INFO_PHYSICAL_DISK: u32 = 9;
    pub const GET_VIRTUAL_DISK_INFO_VHD_PHYSICAL_SECTOR_SIZE: u32 = 10;
    pub const GET_VIRTUAL_DISK_INFO_SMALLEST_SAFE_VIRTUAL_SIZE: u32 = 11;
    pub const GET_VIRTUAL_DISK_INFO_FRAGMENTATION: u32 = 12;
    pub const GET_VIRTUAL_DISK_INFO_IS_LOADED: u32 = 13;
    pub const GET_VIRTUAL_DISK_INFO_VIRTUAL_DISK_ID: u32 = 14;
    pub const GET_VIRTUAL_DISK_INFO_CHANGE_TRACKING_STATE: u32 = 15;

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct GET_VIRTUAL_DISK_INFO {
        pub Version: u32,
        pub u: GET_VIRTUAL_DISK_INFO_u,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub union GET_VIRTUAL_DISK_INFO_u {
        pub Size: GET_VIRTUAL_DISK_INFO_Size,
        pub Identifier: GUID,
        pub ParentIdentifier: GUID,
        pub ParentTimestamp: u32,
        pub VirtualStorageType: VIRTUAL_STORAGE_TYPE,
        pub ProviderSubtype: u32,
        pub Is4kAligned: BOOL,
        pub IsLoaded: BOOL,
        pub VhdPhysicalSectorSize: u32,
        pub SmallestSafeVirtualSize: u64,
        pub FragmentationPercentage: u32,
        pub VirtualDiskId: GUID,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct GET_VIRTUAL_DISK_INFO_Size {
        pub VirtualSize: u64,
        pub PhysicalSize: u64,
        pub BlockSize: u32,
        pub SectorSize: u32,
    }

    #[link(name = "virtdisk")]
    unsafe extern "system" {
        pub fn OpenVirtualDisk(
            virtual_storage_type: &mut VIRTUAL_STORAGE_TYPE,
            path: *const u16,
            virtual_disk_access_mask: u32,
            flags: u32,
            parameters: Option<&mut OPEN_VIRTUAL_DISK_PARAMETERS>,
            handle: &mut RawHandle,
        ) -> u32;

        pub fn AttachVirtualDisk(
            virtual_disk_handle: RawHandle,
            security_descriptor: Option<&mut SECURITY_DESCRIPTOR>,
            flags: u32,
            provider_specific_flags: u32,
            parameters: usize,
            overlapped: Option<&mut OVERLAPPED>,
        ) -> u32;

        pub fn GetVirtualDiskInformation(
            virtual_disk_handle: RawHandle,
            virtual_disk_info_size: &mut u32,
            virtual_disk_info: Option<&mut GET_VIRTUAL_DISK_INFO>,
            size_use: Option<&mut u32>,
        ) -> u32;

        pub fn CreateVirtualDisk(
            virtual_storage_type: &mut VIRTUAL_STORAGE_TYPE,
            path: *const u16,
            virtual_disk_access_mask: u32,
            security_descriptor: Option<&mut SECURITY_DESCRIPTOR>,
            flags: u32,
            provider_specific_flags: u32,
            parameters: Option<&mut CREATE_VIRTUAL_DISK_PARAMETERS>,
            overlapped: Option<&mut OVERLAPPED>,
            handle: &mut RawHandle,
        ) -> u32;
    }
}

#[derive(Debug, MeshPayload)]
/// Handle for an open VHD file.
pub struct Vhd(fs::File);

fn chk_win32(err: u32) -> std::io::Result<()> {
    if err == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(err as i32))
    }
}

impl Vhd {
    fn open(path: &Path, read_only: bool) -> std::io::Result<Self> {
        let mut storage_type = virtdisk::VIRTUAL_STORAGE_TYPE::default();
        // Use a unique ID for each open to avoid virtual disk sharing
        // within VHDMP. In the future, consider taking this as a parameter
        // to support failover.
        let resiliency_guid = Guid::new_random();
        let mut parameters = virtdisk::OPEN_VIRTUAL_DISK_PARAMETERS {
            Version: 2,
            u: virtdisk::OPEN_VIRTUAL_DISK_PARAMETERS_u {
                Version2: virtdisk::OPEN_VIRTUAL_DISK_PARAMETERS_2 {
                    ReadOnly: read_only.into(),
                    ResiliencyGuid: resiliency_guid.into(),
                    GetInfoOnly: 0,
                },
            },
        };
        let mut path16: Vec<_> = path.as_os_str().encode_wide().collect();
        path16.push(0);
        let mut handle = std::ptr::null_mut();

        // SAFETY: All structs are correctly initalized, the path has a null
        // terminator, and we validate the result immediately.
        unsafe {
            chk_win32(virtdisk::OpenVirtualDisk(
                &mut storage_type,
                path16.as_ptr(),
                0,
                0,
                Some(&mut parameters),
                &mut handle,
            ))?;
            Ok(Self(fs::File::from_raw_handle(handle)))
        }
    }

    /// Create a new dynamic VHD
    pub fn create_dynamic(path: &Path, max_size_mb: u64, vhdx: bool) -> std::io::Result<Self> {
        let mut path16: Vec<_> = path.as_os_str().encode_wide().collect();
        path16.push(0);

        let mut storage_type = virtdisk::VIRTUAL_STORAGE_TYPE {
            DeviceId: if vhdx {
                virtdisk::VIRTUAL_STORAGE_TYPE_DEVICE_VHDX
            } else {
                virtdisk::VIRTUAL_STORAGE_TYPE_DEVICE_VHD
            },
            VendorId: virtdisk::VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT.into(),
        };

        // Use a unique ID for each open to avoid virtual disk sharing
        // within VHDMP. In the future, consider taking this as a parameter
        // to support failover.
        let resiliency_guid = Guid::new_random();
        let mut parameters = virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: 2,
            u: virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS_u {
                Version2: virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS_2 {
                    UniqueId: Guid::new_random().into(),
                    MaximumSize: max_size_mb * 1024 * 1024,
                    BlockSizeInBytes: 2 * 1024 * 1024,
                    SectorSizeInBytes: 512,
                    OpenFlags: virtdisk::OPEN_VIRTUAL_DISK_FLAG_CACHED_IO,
                    ResiliencyGuid: resiliency_guid.into(),
                    PhysicalSectorSizeInBytes: 0,
                    ParentPath: std::ptr::null(),
                    SourcePath: std::ptr::null(),
                    ParentVirtualStorageType: virtdisk::VIRTUAL_STORAGE_TYPE::default(),
                    SourceVirtualStorageType: virtdisk::VIRTUAL_STORAGE_TYPE::default(),
                },
            },
        };
        let mut handle = std::ptr::null_mut();

        // SAFETY: All structs are correctly initalized, the path has a null
        // terminator, and we validate the result immediately.
        unsafe {
            chk_win32(virtdisk::CreateVirtualDisk(
                &mut storage_type,
                path16.as_ptr(),
                0,
                None,
                0,
                0,
                Some(&mut parameters),
                None,
                &mut handle,
            ))?;
            Ok(Self(fs::File::from_raw_handle(handle)))
        }
    }

    /// Create a new differencing VHD
    pub fn create_diff(path: &Path, parent_path: &Path) -> std::io::Result<Self> {
        let mut storage_type = virtdisk::VIRTUAL_STORAGE_TYPE::default();

        let mut path16: Vec<_> = path.as_os_str().encode_wide().collect();
        path16.push(0);

        let mut parent_path16: Vec<_> = parent_path.as_os_str().encode_wide().collect();
        parent_path16.push(0);

        // Use a unique ID for each open to avoid virtual disk sharing
        // within VHDMP. In the future, consider taking this as a parameter
        // to support failover.
        let resiliency_guid = Guid::new_random();
        let mut parameters = virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: 2,
            u: virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS_u {
                Version2: virtdisk::CREATE_VIRTUAL_DISK_PARAMETERS_2 {
                    ParentPath: parent_path16.as_ptr(),
                    OpenFlags: virtdisk::OPEN_VIRTUAL_DISK_FLAG_CACHED_IO,
                    ResiliencyGuid: resiliency_guid.into(),
                    UniqueId: Guid::ZERO.into(),
                    MaximumSize: 0,
                    BlockSizeInBytes: 0,
                    SectorSizeInBytes: 0,
                    PhysicalSectorSizeInBytes: 0,
                    SourcePath: std::ptr::null_mut(),
                    ParentVirtualStorageType: virtdisk::VIRTUAL_STORAGE_TYPE::default(),
                    SourceVirtualStorageType: virtdisk::VIRTUAL_STORAGE_TYPE::default(),
                },
            },
        };

        let mut handle = std::ptr::null_mut();

        // SAFETY: All structs are correctly initalized, the path has a null
        // terminator, and we validate the result immediately.
        unsafe {
            chk_win32(virtdisk::CreateVirtualDisk(
                &mut storage_type,
                path16.as_ptr(),
                0,
                None,
                0,
                0,
                Some(&mut parameters),
                None,
                &mut handle,
            ))?;
            Ok(Self(fs::File::from_raw_handle(handle)))
        }
    }

    /// Configure the VHD for raw access
    pub fn attach_for_raw_access(&self, read_only: bool) -> std::io::Result<()> {
        let mut flags = virtdisk::ATTACH_VIRTUAL_DISK_FLAG_NO_LOCAL_HOST;
        if read_only {
            flags |= virtdisk::ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY;
        }
        // SAFETY: We are guaranteed to be holding an open handle, and we
        // validate the result immediately.
        unsafe {
            chk_win32(virtdisk::AttachVirtualDisk(
                self.0.as_raw_handle(),
                None,
                flags,
                0,
                0,
                None,
            ))?;
        }
        Ok(())
    }

    fn info_static(&self, info_type: u32) -> std::io::Result<virtdisk::GET_VIRTUAL_DISK_INFO> {
        // SAFETY: We are guaranteed to be holding an open handle, and we
        // validate the result immediately.
        unsafe {
            let mut info = virtdisk::GET_VIRTUAL_DISK_INFO {
                Version: info_type,
                ..std::mem::zeroed()
            };
            let mut size = size_of_val(&info) as u32;
            chk_win32(virtdisk::GetVirtualDiskInformation(
                self.0.as_raw_handle(),
                &mut size,
                Some(&mut info),
                None,
            ))?;

            Ok(info)
        }
    }

    fn get_size(&self) -> std::io::Result<virtdisk::GET_VIRTUAL_DISK_INFO_Size> {
        // SAFETY: Accessing the right union field for this call.
        unsafe {
            Ok(self
                .info_static(virtdisk::GET_VIRTUAL_DISK_INFO_SIZE)?
                .u
                .Size)
        }
    }

    fn get_physical_sector_size(&self) -> std::io::Result<u32> {
        // SAFETY: Accessing the right union field for this call.
        unsafe {
            Ok(self
                .info_static(virtdisk::GET_VIRTUAL_DISK_INFO_VHD_PHYSICAL_SECTOR_SIZE)?
                .u
                .VhdPhysicalSectorSize)
        }
    }

    fn get_disk_id(&self) -> std::io::Result<Guid> {
        // SAFETY: Accessing the right union field for this call.
        unsafe {
            Ok(self
                .info_static(virtdisk::GET_VIRTUAL_DISK_INFO_VIRTUAL_DISK_ID)?
                .u
                .VirtualDiskId
                .into())
        }
    }
}

#[derive(MeshPayload)]
/// Configuration to open a VHDMP disk.
pub struct OpenVhdmpDiskConfig(pub Vhd);

impl ResourceId<DiskHandleKind> for OpenVhdmpDiskConfig {
    const ID: &'static str = "vhdmp";
}

/// Resolver for VHDMP disks.
pub struct VhdmpDiskResolver;
declare_static_resolver!(VhdmpDiskResolver, (DiskHandleKind, OpenVhdmpDiskConfig));

#[derive(Debug, Error)]
/// Errors that can occur when resolving a VHDMP disk.
pub enum ResolveVhdmpDiskError {
    #[error("failed to open VHD")]
    /// Error from VHDMP when opening the disk.
    Vhdmp(#[source] Error),
    #[error("invalid disk")]
    /// The disk is invalid.
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

impl ResolveResource<DiskHandleKind, OpenVhdmpDiskConfig> for VhdmpDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveVhdmpDiskError;

    fn resolve(
        &self,
        rsrc: OpenVhdmpDiskConfig,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        ResolvedDisk::new(
            VhdmpDisk::new(rsrc.0, input.read_only).map_err(ResolveVhdmpDiskError::Vhdmp)?,
        )
        .map_err(ResolveVhdmpDiskError::InvalidDisk)
    }
}

/// Implementation of [`DiskIo`] for VHD and VHDX files, using the VHDMP driver
/// as the parser.
#[derive(Debug, Inspect)]
pub struct VhdmpDisk {
    #[inspect(flatten)]
    vhd: FileDisk,
    /// Lock uses to serialize IOs, since FileDisk currently cannot handle
    /// multiple concurrent IOs on files opened with FILE_FLAG_OVERLAPPED on
    /// Windows (and the VHDMP handle is opened with FILE_FLAG_OVERLAPPED).
    #[inspect(skip)]
    io_lock: futures::lock::Mutex<()>,
    disk_id: Guid,
}

#[derive(Debug, Error)]
/// Errors that can occur when working with VHDMP disks.
pub enum Error {
    #[error("failed to open VHD")]
    /// Error opening the disk
    Open(#[source] std::io::Error),
    #[error("failed to create VHD")]
    /// Error creating the disk
    Create(#[source] std::io::Error),
    #[error("failed to attach VHD")]
    /// Error attaching the disk
    Attach(#[source] std::io::Error),
    #[error("failed to query VHD metadata")]
    /// Error querying disk metadata
    Query(#[source] std::io::Error),
}

impl VhdmpDisk {
    /// Opens a VHD for use with [`Self::new()`].
    pub fn open_vhd(path: &Path, read_only: bool) -> Result<Vhd, Error> {
        let vhd = Vhd::open(path, read_only).map_err(Error::Open)?;

        // N.B. This must be attached here and not later in a worker process
        //      since this operation may require impersonation, which is
        //      prohibited from a sandboxed process.
        vhd.attach_for_raw_access(read_only)
            .map_err(Error::Attach)?;
        Ok(vhd)
    }

    /// Creates a disk from an open VHD handle. `vhd` should have been opened via [`Self::open_vhd()`].
    pub fn new(vhd: Vhd, read_only: bool) -> Result<Self, Error> {
        let size = vhd.get_size().map_err(Error::Query)?;
        let disk_id = vhd.get_disk_id().map_err(Error::Query)?;
        let metadata = disk_file::Metadata {
            disk_size: size.VirtualSize,
            sector_size: size.SectorSize,
            physical_sector_size: vhd.get_physical_sector_size().map_err(Error::Query)?,
            read_only,
        };
        let vhd = FileDisk::with_metadata(vhd.0, metadata);

        Ok(Self {
            vhd,
            io_lock: Default::default(),
            disk_id,
        })
    }
}

impl DiskIo for VhdmpDisk {
    fn disk_type(&self) -> &str {
        "vhdmp"
    }

    fn sector_count(&self) -> u64 {
        self.vhd.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.vhd.sector_size()
    }

    fn is_read_only(&self) -> bool {
        self.vhd.is_read_only()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        Some(self.disk_id.into())
    }

    fn physical_sector_size(&self) -> u32 {
        self.vhd.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.vhd.is_fua_respected()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let _locked = self.io_lock.lock().await;
        self.vhd.read(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let _locked = self.io_lock.lock().await;
        self.vhd.write(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        let _locked = self.io_lock.lock().await;
        self.vhd.flush().await
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
        disk_backend::UnmapBehavior::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::VhdmpDisk;
    use disk_backend::DiskError;
    use disk_backend::DiskIo;
    use disk_vhd1::Vhd1Disk;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use std::io::Write;
    use tempfile::TempPath;

    fn make_test_vhd() -> TempPath {
        let mut f = tempfile::Builder::new().suffix(".vhd").tempfile().unwrap();
        let size = 0x300000;
        f.write_all(&vec![0u8; size]).unwrap();
        Vhd1Disk::make_fixed(f.as_file()).unwrap();
        f.into_temp_path()
    }

    #[test]
    fn open_readonly() {
        let path = make_test_vhd();
        let _vhd = VhdmpDisk::open_vhd(path.as_ref(), true).unwrap();
        let _vhd = VhdmpDisk::open_vhd(path.as_ref(), true).unwrap();
        let _vhd = VhdmpDisk::open_vhd(path.as_ref(), false).unwrap_err();
    }

    #[async_test]
    async fn test_invalid_lba() {
        let path = make_test_vhd();
        let vhd = VhdmpDisk::open_vhd(path.as_ref(), true).unwrap();
        let disk = VhdmpDisk::new(vhd, true).unwrap();
        let gm = GuestMemory::allocate(512);
        match disk
            .read_vectored(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&gm),
                0x10000000,
            )
            .await
        {
            Err(DiskError::IllegalBlock) => {}
            r => panic!("unexpected result: {:?}", r),
        }
    }
}
