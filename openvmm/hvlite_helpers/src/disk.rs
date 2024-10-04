// Copyright (C) Microsoft Corporation. All rights reserved.

//! Guest disk helpers.

use std::path::Path;
use vm_resource::kind::DiskHandleKind;
use vm_resource::Resource;

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
        _ => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;

            Resource::new(disk_backend_resources::FileDiskHandle(file))
        }
    })
}
