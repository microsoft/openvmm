// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest disk helpers.

use anyhow::Context;
use std::path::Path;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

fn disk_open_error(path: &Path, verb: &str) -> String {
    let mut msg = format!("{verb} '{}'", path.display());

    // On windows, attempt to detect we ran under wsl by reading the WSLENV and
    // bail out with a helpful hint that it needs to be a windows path.
    if cfg!(windows) && std::env::var_os("WSLENV").is_some() {
        msg += ". Linux paths are not supported when running Windows executables \
                under WSL, make sure the path is a valid Windows path \
                (use `wslpath -w` to convert)";
    }

    msg
}

/// Options for opening a disk file.
#[derive(Clone, Copy)]
pub struct OpenDiskOptions {
    /// Open the disk as read-only.
    pub read_only: bool,
    /// Bypass the OS page cache for direct disk I/O.
    pub direct: bool,
}

/// Opens the resources needed for using a disk from a file at `path`.
///
/// If the file ends with .vhd and is a fixed VHD1, it will be opened using
/// the user-mode VHD parser. Otherwise, if the file ends with .vhd or
/// .vhdx, the file will be opened using the kernel-mode VHD parser.
pub fn open_disk_type(
    path: &Path,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let read_only = options.read_only;
    let ensure_no_direct = |ext| {
        if options.direct {
            anyhow::bail!("direct I/O is not supported for {ext} files");
        };
        Ok(())
    };
    Ok(match path.extension().and_then(|s| s.to_str()) {
        Some("vhd") => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)
                .with_context(|| disk_open_error(path, "failed to open"))?;

            match disk_vhd1::Vhd1Disk::open_fixed(file, read_only) {
                Ok(vhd) => {
                    ensure_no_direct("fixed .vhd")?;
                    Resource::new(disk_backend_resources::FixedVhd1DiskHandle(
                        vhd.into_inner(),
                    ))
                }
                Err(disk_vhd1::OpenError::NotFixed) => {
                    #[cfg(windows)]
                    {
                        Resource::new(disk_vhdmp::OpenVhdmpDiskConfig(
                            disk_vhdmp::VhdmpDisk::options()
                                .read_only(read_only)
                                .cached_io(!options.direct)
                                .open(path)
                                .with_context(|| disk_open_error(path, "failed to open"))?,
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
                    disk_vhdmp::VhdmpDisk::options()
                        .read_only(read_only)
                        .cached_io(!options.direct)
                        .open(path)
                        .with_context(|| disk_open_error(path, "failed to open"))?,
                ))
            }
            #[cfg(not(windows))]
            anyhow::bail!("VHDX not supported on Linux");
        }
        Some("iso") if !read_only => {
            anyhow::bail!("iso file cannot be opened as read/write")
        }
        Some("vmgs") => {
            ensure_no_direct(".vmgs")?;
            // VMGS files are fixed VHD1s. Don't bother to validate the footer
            // here; let the resource resolver do that later.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)
                .with_context(|| disk_open_error(path, "failed to open"))?;

            Resource::new(disk_backend_resources::FixedVhd1DiskHandle(file))
        }
        _ => open_raw_disk(path, options, None)?,
    })
}

/// Create and open the resources needed for using a disk from a file at `path`.
pub fn create_disk_type(
    path: &Path,
    size: u64,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    Ok(match path.extension().and_then(|s| s.to_str()) {
        Some("vhd") | Some("vmgs") => {
            if options.direct {
                anyhow::bail!("direct I/O is not supported for VHD files");
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| disk_open_error(path, "failed to create"))?;

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
        _ => open_raw_disk(path, options, Some(size))?,
    })
}

/// Open or create a raw file or block device, returning the appropriate
/// disk resource for the current platform.
fn open_raw_disk(
    path: &Path,
    options: OpenDiskOptions,
    size: Option<u64>,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    if options.direct && !cfg!(target_os = "linux") {
        anyhow::bail!("direct I/O is only supported on Linux");
    }

    let create = size.is_some();
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(!options.read_only);
    if create {
        opts.create(true).truncate(true);
    }

    #[cfg(target_os = "linux")]
    if options.direct {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECT);
    }

    let verb = if create {
        "failed to create"
    } else {
        "failed to open"
    };
    let file = opts
        .open(path)
        .with_context(|| disk_open_error(path, verb))?;

    if let Some(size) = size {
        file.set_len(size)?;
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Resource::new(
            disk_backend_resources::BlockDeviceDiskHandle { file },
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(Resource::new(disk_backend_resources::FileDiskHandle(file)))
    }
}
