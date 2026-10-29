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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;

    fn has_raw_error(error: &io::Error, errors: &[i32]) -> bool {
        error
            .raw_os_error()
            .is_some_and(|error| errors.contains(&error))
    }

    fn reflink_is_unsupported(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::Unsupported
            || has_raw_error(
                error,
                &[libc::EINVAL, libc::ENOTTY, libc::EOPNOTSUPP, libc::EXDEV],
            )
    }

    fn sparse_seek_is_unsupported(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::Unsupported
            || has_raw_error(error, &[libc::EINVAL, libc::EOPNOTSUPP])
    }

    enum SparseSeekResult {
        Unsupported,
        Offset(Option<u64>),
    }

    fn seek_if_supported(result: io::Result<Option<u64>>) -> SparseSeekResult {
        match result {
            Ok(offset) => SparseSeekResult::Offset(offset),
            Err(error) if sparse_seek_is_unsupported(&error) => SparseSeekResult::Unsupported,
            Err(error) => panic!("sparse seek failed: {error}"),
        }
    }

    #[test]
    fn test_seek_rejects_offsets_above_i64_max() {
        let file = tempfile::tempfile().unwrap();
        let offset = i64::MAX as u64 + 1;

        assert_eq!(
            seek_data(&file, offset).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            seek_hole(&file, offset).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn test_seek_returns_none_at_eof() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"data").unwrap();
        file.sync_all().unwrap();
        let eof = file.metadata().unwrap().len();

        match seek_data(&file, eof) {
            Ok(offset) => assert_eq!(offset, None),
            Err(error) if sparse_seek_is_unsupported(&error) => {}
            Err(error) => panic!("SEEK_DATA at EOF failed: {error}"),
        }
        match seek_hole(&file, eof) {
            Ok(offset) => assert_eq!(offset, None),
            Err(error) if sparse_seek_is_unsupported(&error) => {}
            Err(error) => panic!("SEEK_HOLE at EOF failed: {error}"),
        }
    }

    #[test]
    fn test_seek_traverses_sparse_extents() {
        const SECOND_DATA_OFFSET: u64 = 1024 * 1024;
        const FILE_LEN: u64 = 2 * SECOND_DATA_OFFSET;

        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"first extent").unwrap();
        file.seek(SeekFrom::Start(SECOND_DATA_OFFSET)).unwrap();
        file.write_all(b"second extent").unwrap();
        file.set_len(FILE_LEN).unwrap();
        file.sync_all().unwrap();

        let mut ranges = Vec::new();
        let mut offset = 0;
        loop {
            let SparseSeekResult::Offset(next_data) = seek_if_supported(seek_data(&file, offset))
            else {
                return;
            };
            let Some(data) = next_data else {
                break;
            };
            assert!(data >= offset);

            let SparseSeekResult::Offset(next_hole) = seek_if_supported(seek_hole(&file, data))
            else {
                return;
            };
            let hole = next_hole.expect("a data extent must be followed by a hole");
            assert!(hole > data);
            assert!(hole <= FILE_LEN);
            ranges.push((data, hole));

            if hole == FILE_LEN {
                break;
            }
            offset = hole;
        }

        assert!(ranges.iter().any(|&(start, end)| start == 0 && 0 < end));
        assert!(
            ranges
                .iter()
                .any(|&(start, end)| start <= SECOND_DATA_OFFSET && SECOND_DATA_OFFSET < end)
        );
    }

    #[test]
    fn test_reflink_copies_contents_and_is_independently_writable() {
        const CONTENTS: &[u8] = b"contents shared only until either file is written";

        let directory = tempfile::tempdir().unwrap();
        let mut source = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("source"))
            .unwrap();
        let mut destination = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("destination"))
            .unwrap();
        source.write_all(CONTENTS).unwrap();
        source.sync_all().unwrap();

        if let Err(error) = reflink(&source, &destination) {
            assert!(
                reflink_is_unsupported(&error),
                "reflink failed unexpectedly: {error}"
            );
            return;
        }

        let mut cloned_contents = Vec::new();
        destination.seek(SeekFrom::Start(0)).unwrap();
        destination.read_to_end(&mut cloned_contents).unwrap();
        assert_eq!(cloned_contents, CONTENTS);

        destination.seek(SeekFrom::Start(0)).unwrap();
        destination.write_all(b"D").unwrap();
        destination.sync_all().unwrap();

        let mut source_contents = Vec::new();
        source.seek(SeekFrom::Start(0)).unwrap();
        source.read_to_end(&mut source_contents).unwrap();
        assert_eq!(source_contents, CONTENTS);

        source.seek(SeekFrom::Start(1)).unwrap();
        source.write_all(b"S").unwrap();
        source.sync_all().unwrap();

        let mut destination_contents = Vec::new();
        destination.seek(SeekFrom::Start(0)).unwrap();
        destination.read_to_end(&mut destination_contents).unwrap();
        let mut expected_destination = CONTENTS.to_vec();
        expected_destination[0] = b'D';
        assert_eq!(destination_contents, expected_destination);
    }
}
