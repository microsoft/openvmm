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
/// the user-mode VHD parser. Otherwise, if the file ends with .vhd, the
/// file will be opened using the kernel-mode VHD parser (Windows only).
///
/// If the file ends with .vhdx, the kernel-mode VHD parser is used on
/// Windows. On Linux, the pure-Rust VHDX parser is used, with automatic
/// parent-locator walking for differencing chains.
///
/// If the file ends with .vmdk, the VMDK backend is used for all platforms.
pub async fn open_disk_type(
    path: &Path,
    options: OpenDiskOptions,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        let metadata =
            std::fs::metadata(path).with_context(|| disk_open_error(path, "failed to inspect"))?;
        anyhow::ensure!(
            metadata.is_file() || metadata.file_type().is_block_device(),
            "disk backing must be a regular file or block device"
        );
        if metadata.file_type().is_block_device() {
            return open_raw_disk(path, options, None);
        }
    }
    let read_only = options.read_only;
    let ensure_no_direct = |ext| {
        if options.direct {
            anyhow::bail!("direct I/O is not supported for {ext} files");
        };
        Ok(())
    };
    Ok(match path.extension().and_then(|s| s.to_str()) {
        Some("vmdk") => {
            ensure_no_direct(".vmdk")?;
            disk_vmdk::open_vmdk(path, read_only)
                .with_context(|| disk_open_error(path, "failed to open"))?
        }
        Some("vhd") => {
            let file = open_disk_file(
                path,
                OpenDiskOptions {
                    direct: false,
                    ..options
                },
                None,
            )?;

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
            {
                ensure_no_direct(".vhdx")?;
                disklayer_vhdx::chain::open_vhdx_chain(path, read_only).await?
            }
        }
        Some("iso") if !read_only => {
            anyhow::bail!("iso file cannot be opened as read/write")
        }
        Some("vmgs") => {
            ensure_no_direct(".vmgs")?;
            // VMGS files are fixed VHD1s. Don't bother to validate the footer
            // here; let the resource resolver do that later.
            let file = open_disk_file(path, options, None)?;

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
        Some("vmdk") => {
            anyhow::bail!("creating vmdk not supported")
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
    let file = open_disk_file(path, options, size)?;
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

fn open_disk_file(
    path: &Path,
    options: OpenDiskOptions,
    size: Option<u64>,
) -> anyhow::Result<std::fs::File> {
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
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }

    let verb = if create {
        "failed to create"
    } else {
        "failed to open"
    };
    let file = opts
        .open(path)
        .with_context(|| disk_open_error(path, verb))?;

    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::FcntlArg;
        use nix::fcntl::OFlag;
        use nix::fcntl::fcntl;
        use std::os::unix::fs::FileTypeExt;
        let metadata = file
            .metadata()
            .with_context(|| disk_open_error(path, "failed to inspect opened disk"))?;
        anyhow::ensure!(
            metadata.is_file() || metadata.file_type().is_block_device(),
            "disk backing must be a regular file or block device"
        );
        let mut flags = OFlag::from_bits_retain(
            fcntl(&file, FcntlArg::F_GETFL)
                .with_context(|| disk_open_error(path, "failed to read disk I/O flags"))?,
        );
        flags.remove(OFlag::O_NONBLOCK);
        flags.set(OFlag::O_DIRECT, options.direct);
        fcntl(&file, FcntlArg::F_SETFL(flags))
            .with_context(|| disk_open_error(path, "failed to set disk I/O flags"))?;
    }
    if let Some(size) = size {
        file.set_len(size)?;
    }
    Ok(file)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use futures::executor::block_on;
    use nix::fcntl::FcntlArg;
    use nix::fcntl::OFlag;
    use nix::fcntl::fcntl;
    use test_with_tracing::test;

    #[test]
    fn raw_file_open_preserves_access_and_direct_flags() {
        let backing = tempfile::NamedTempFile::new().unwrap();
        backing.as_file().set_len(4096).unwrap();
        for direct in [false, true] {
            for read_only in [false, true] {
                let file =
                    open_disk_file(backing.path(), OpenDiskOptions { direct, read_only }, None)
                        .unwrap();
                let flags = OFlag::from_bits_retain(fcntl(&file, FcntlArg::F_GETFL).unwrap());
                assert_eq!(flags.contains(OFlag::O_DIRECT), direct);
                assert!(!flags.contains(OFlag::O_NONBLOCK));
                assert_eq!(
                    flags & OFlag::O_ACCMODE,
                    if read_only {
                        OFlag::O_RDONLY
                    } else {
                        OFlag::O_RDWR
                    }
                );
            }
        }
    }

    #[test]
    fn rejects_non_disk_paths_before_opening() {
        use nix::sys::stat::Mode;

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo.vhdx");
        nix::unistd::mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        for path in [directory.path(), fifo.as_path(), Path::new("/dev/null")] {
            for read_only in [false, true] {
                let error = block_on(open_disk_type(
                    path,
                    OpenDiskOptions {
                        direct: false,
                        read_only,
                    },
                ))
                .unwrap_err();
                assert!(error.to_string().contains("regular file"), "{error:#}");
            }
        }
    }

    #[test]
    #[ignore = "requires a disposable device-mapper fixture in OPENVMM_BOUNCE_TEST_BLOCK_PATH"]
    fn block_devices_ignore_image_suffixes_and_preserve_flags() {
        use std::os::unix::fs::FileTypeExt;

        let path = std::env::var("OPENVMM_BOUNCE_TEST_BLOCK_PATH").unwrap();
        assert!(
            Path::new(&path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("openvmm-bounce-test-")
        );
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("mapped-disk.vhdx");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        for direct in [false, true] {
            for read_only in [false, true] {
                let options = OpenDiskOptions { direct, read_only };
                let file = open_disk_file(&alias, options, None).unwrap();
                assert!(file.metadata().unwrap().file_type().is_block_device());
                let flags = OFlag::from_bits_retain(fcntl(&file, FcntlArg::F_GETFL).unwrap());
                assert_eq!(flags.contains(OFlag::O_DIRECT), direct);
                assert!(!flags.contains(OFlag::O_NONBLOCK));
                assert_eq!(
                    flags & OFlag::O_ACCMODE,
                    if read_only {
                        OFlag::O_RDONLY
                    } else {
                        OFlag::O_RDWR
                    }
                );
                assert_eq!(
                    block_on(open_disk_type(&alias, options)).unwrap().id(),
                    "block"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;

    // Creates a minimal flat VMDK descriptor and source file for routing tests.
    fn create_test_vmdk() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.bin"), [0; 512]).unwrap();
        let descriptor = directory.path().join("disk.vmdk");
        std::fs::write(
            &descriptor,
            "# Disk DescriptorFile\n\
             version=1\n\
             parentCID=ffffffff\n\
             createType=\"monolithicFlat\"\n\
             # Extent description\n\
             RW 1 FLAT \"source.bin\" 0\n",
        )
        .unwrap();
        (directory, descriptor)
    }

    #[async_test]
    async fn opens_flat_vmdk_read_only() {
        let (_directory, descriptor) = create_test_vmdk();
        open_disk_type(
            &descriptor,
            OpenDiskOptions {
                read_only: true,
                direct: false,
            },
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn rejects_vmdk_write_direct_io_and_creation() {
        let (_directory, descriptor) = create_test_vmdk();
        // Writable VMDK opens are not supported.
        assert!(
            open_disk_type(
                &descriptor,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .await
            .is_err()
        );
        // VMDK sources cannot use direct I/O.
        assert!(
            open_disk_type(
                &descriptor,
                OpenDiskOptions {
                    read_only: true,
                    direct: true,
                },
            )
            .await
            .is_err()
        );
        // The generic disk creator must not treat VMDK as a raw file.
        assert!(
            create_disk_type(
                &descriptor,
                512,
                OpenDiskOptions {
                    read_only: false,
                    direct: false,
                },
            )
            .is_err()
        );
    }
}
