// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest disk helpers.

use std::path::Path;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

/// Opens the resources needed for using a disk from a file at `path`.
///
/// If the file ends with .vhd and is a fixed VHD1, it will be opened using
/// the user-mode VHD parser. Otherwise, if the file ends with .vhd or
/// .vhdx, the file will be opened using the kernel-mode VHD parser.
pub fn open_disk_type(path: &Path, read_only: bool) -> anyhow::Result<Resource<DiskHandleKind>> {
    Ok(match path.extension().and_then(|s| s.to_str()) {
        Some("vhd") => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;

            match disk_vhd1::Vhd1Disk::open_fixed(file, read_only) {
                Ok(vhd) => Resource::new(disk_backend_resources::FixedVhd1DiskHandle(
                    vhd.into_inner(),
                )),
                Err(disk_vhd1::OpenError::NotFixed) => {
                    #[cfg(windows)]
                    {
                        Resource::new(disk_vhdmp::OpenVhdmpDiskConfig(
                            disk_vhdmp::VhdmpDisk::open_vhd(path, read_only)?,
                        ))
                    }
                    #[cfg(not(windows))]
                    anyhow::bail!("non-fixed VHD not supported on Linux");
                }
                Err(err) => return Err(err.into()),
            }
        }
        Some("vhdx") => {
            #[cfg(windows)]
            {
                Resource::new(disk_vhdmp::OpenVhdmpDiskConfig(
                    disk_vhdmp::VhdmpDisk::open_vhd(path, read_only)?,
                ))
            }
            #[cfg(not(windows))]
            anyhow::bail!("VHDX not supported on Linux");
        }
        Some("iso") if !read_only => {
            anyhow::bail!("iso file cannot be opened as read/write")
        }
        Some("vmgs") => {
            // VMGS files are fixed VHD1s. Don't bother to validate the footer
            // here; let the resource resolver do that later.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;

            Resource::new(disk_backend_resources::FixedVhd1DiskHandle(file))
        }
        _ => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;

            Resource::new(disk_backend_resources::FileDiskHandle(file))
        }
    })
}

/// Create and open the resources needed for using a disk from a file at `path`.
pub fn create_disk_type(path: &Path, size: u64) -> anyhow::Result<Resource<DiskHandleKind>> {
    Ok(match path.extension().and_then(|s| s.to_str()) {
        Some("vhd") | Some("vmgs") => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(path)?;

            file.set_len(size)?;
            disk_vhd1::Vhd1Disk::make_fixed(&file)?;
            Resource::new(disk_backend_resources::FixedVhd1DiskHandle(file))
        }
        Some("vhdx") => {
            anyhow::bail!("creating vhdx not supported")
        }
        Some("iso") => {
            anyhow::bail!("creating iso not supported")
        }
        _ => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(path)?;

            file.set_len(size)?;
            Resource::new(disk_backend_resources::FileDiskHandle(file))
        }
    })
}
