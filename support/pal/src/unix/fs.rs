// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::File;
use std::io;

/// Clones all file extents into an independently writable destination.
pub fn reflink(source: &File, destination: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: Both arguments are live regular-file descriptors and FICLONE
        // does not retain either descriptor after the call.
        let result =
            unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file extent cloning is unsupported on this platform",
        ))
    }
}

/// Finds the next data extent at or after `offset`.
pub fn seek_data(file: &File, offset: u64) -> io::Result<Option<u64>> {
    seek_sparse(file, offset, libc::SEEK_DATA)
}

/// Finds the next hole at or after `offset`.
pub fn seek_hole(file: &File, offset: u64) -> io::Result<Option<u64>> {
    seek_sparse(file, offset, libc::SEEK_HOLE)
}

fn seek_sparse(file: &File, offset: u64, whence: i32) -> io::Result<Option<u64>> {
    use std::os::fd::AsRawFd;

    let offset = i64::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds i64"))?;
    // SAFETY: The descriptor is live and `whence` is selected internally from
    // the platform's SEEK_DATA and SEEK_HOLE constants.
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result >= 0 {
        Ok(Some(result as u64))
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENXIO) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}
