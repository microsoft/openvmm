// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the flat VMDK backend.

use super::*;
use disk_backend::Disk;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use std::fs::OpenOptions;
use std::path::PathBuf;
use test_with_tracing::test;

/// Builds a flat VMDK descriptor with the given disk type and extent entries.
fn descriptor(create_type: &str, extents: &str) -> String {
    format!(
        "# Disk DescriptorFile\n\
         version=1\n\
         CID=fffffffe\n\
         parentCID=ffffffff\n\
         createType=\"{create_type}\"\n\
         \n\
         # Extent description\n\
         {extents}\n\
         \n\
         # The Disk Data Base\n\
         #DDB\n\
         ddb.adapterType = \"ide\"\n"
    )
}

/// Writes `disk.vmdk` in the test directory and returns its path.
fn write_descriptor(directory: &tempfile::TempDir, contents: &str) -> PathBuf {
    let descriptor_file_name = "disk.vmdk";
    let path = directory.path().join(descriptor_file_name);
    std::fs::write(&path, contents).unwrap();

    path
}

/// Opens a descriptor and wraps the validated flat backend in a [`Disk`].
fn open_disk(path: &Path) -> Disk {
    let handle = open_flat_vmdk(path, true).unwrap();

    Disk::new(FlatVmdkDisk::new(handle).unwrap()).unwrap()
}

/// Reads sectors through the disk interface and returns a host copy of the bytes.
async fn read_sectors(disk: &Disk, start_sector: u64, sector_count: usize) -> Vec<u8> {
    // Allocate a guest buffer covering the requested sectors.
    let byte_count = sector_count * SECTOR_SIZE as usize;
    let memory = GuestMemory::allocate(byte_count);

    // Read the disk range into guest memory.
    disk.read_vectored(
        &OwnedRequestBuffers::linear(0, byte_count, true).buffer(&memory),
        start_sector,
    )
    .await
    .unwrap();

    // Copy the guest buffer into host memory for test assertions.
    let mut data = vec![0; byte_count];
    memory.read_at(0, &mut data).unwrap();

    data
}

/// Reads sectors directly from the backend, preserving read errors for assertions.
///
/// Unlike [`read_sectors`], this leaves backend state accessible to the caller.
async fn read_backend_sectors(
    disk: &FlatVmdkDisk,
    start_sector: u64,
    sector_count: usize,
) -> Result<Vec<u8>, DiskError> {
    // Allocate writable guest memory for the requested sectors.
    let byte_count = sector_count * SECTOR_SIZE as usize;
    let memory = GuestMemory::allocate(byte_count);

    // Propagate read errors so tests can check failure behavior.
    disk.read_vectored(
        &OwnedRequestBuffers::linear(0, byte_count, true).buffer(&memory),
        start_sector,
    )
    .await?;

    // Copy successful reads into host memory for assertions.
    let mut data = vec![0; byte_count];
    memory.read_at(0, &mut data).unwrap();

    Ok(data)
}

/// Asserts that opening fails with the expected unsafe path and rejection reason.
fn assert_unsafe_path(
    result: Result<FlatVmdkDiskHandle, OpenError>,
    expected_path: &Path,
    expected_reason: &str,
) {
    match result {
        Err(OpenError::UnsafePath { path, reason }) => {
            assert_eq!(path, expected_path);
            assert_eq!(reason, expected_reason);
        }
        Err(error) => panic!("expected an unsafe path error, got {error}"),
        Ok(_) => panic!(
            "unsafe path unexpectedly accepted: {}",
            expected_path.display()
        ),
    }
}

/// Checks that an empty descriptor path is rejected, not treated as the current directory.
#[test]
fn rejects_empty_descriptor_path() {
    assert_unsafe_path(
        open_flat_vmdk(Path::new(""), true),
        Path::new(""),
        "path must not be empty",
    );
}

/// Checks that descriptor paths reject parent traversal with either separator style.
#[test]
fn rejects_parent_traversal_in_descriptor_paths() {
    let reason = "'.' and '..' path components are not supported";

    for unsafe_path in ["../disk.vmdk", r"..\disk.vmdk"] {
        assert_unsafe_path(
            open_flat_vmdk(Path::new(unsafe_path), true),
            Path::new(unsafe_path),
            reason,
        );
    }
}

/// Checks that all extent paths are validated before any source file is opened.
///
/// The first extent names a directory; the second contains parent traversal.
/// The path error must take precedence over the non-file source error.
#[test]
fn rejects_parent_traversal_in_extents_before_opening_sources() {
    // Use a directory as the first source to detect premature source opening.
    let directory = tempfile::tempdir().unwrap();
    let non_regular_source = "source-directory";
    std::fs::create_dir(directory.path().join(non_regular_source)).unwrap();
    let reason = "'.' and '..' path components are not supported";

    // Put an unsafe second extent after the directory source for each separator style.
    for unsafe_path in ["../source.bin", r"..\source.bin"] {
        let descriptor_path = write_descriptor(
            &directory,
            &descriptor(
                "twoGbMaxExtentFlat",
                &format!("RW 1 FLAT \"{non_regular_source}\" 0\nRW 1 FLAT \"{unsafe_path}\" 0"),
            ),
        );

        assert_unsafe_path(
            open_flat_vmdk(&descriptor_path, true),
            Path::new(unsafe_path),
            reason,
        );
    }
}

/// Checks relative path resolution and permits dots within ordinary file names.
///
/// Absolute and relative descriptor paths must both resolve extent paths from
/// the descriptor's directory and read the same source bytes.
#[async_test]
async fn resolves_relative_descriptor_and_extent_paths() {
    // Create the fixture beneath the current directory so the descriptor
    // can be opened using either an absolute or relative path.
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir_in(&cwd).unwrap();

    // Use periods in ordinary directory and file names to distinguish them
    // from rejected `.` and `..` path components.
    let source_directory_name = "images.with.dots";
    let source_file_name = ".source.bin";
    let source_directory = directory.path().join(source_directory_name);
    std::fs::create_dir(&source_directory).unwrap();
    std::fs::write(source_directory.join(source_file_name), [0x25; 512]).unwrap();

    // The relative extent path is resolved from the descriptor directory.
    let descriptor_path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 1 FLAT \"{source_directory_name}/{source_file_name}\" 0"),
        ),
    );

    // Both descriptor-path forms must resolve to the same source data.
    let relative_descriptor_path = descriptor_path.strip_prefix(&cwd).unwrap();
    for descriptor_path in [descriptor_path.as_path(), relative_descriptor_path] {
        let disk = open_disk(descriptor_path);
        assert_eq!(read_sectors(&disk, 0, 1).await, [0x25; 512]);
    }
}

/// Checks that missing descriptors and sources report their paths and `NotFound`.
#[test]
fn resolution_errors_preserve_missing_paths() {
    // Leave both the descriptor path and its named source missing.
    let directory = tempfile::tempdir().unwrap();
    let missing_descriptor_file_name = "missing.vmdk";
    let missing_source_file_name = "missing.bin";
    let missing_descriptor = directory.path().join(missing_descriptor_file_name);

    // Create a separate descriptor that references the missing source.
    let descriptor_path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 1 FLAT \"{missing_source_file_name}\" 0"),
        ),
    );

    // Source errors report the path relative to the resolved descriptor directory.
    let missing_source = fs_err::canonicalize(directory.path())
        .unwrap()
        .join(missing_source_file_name);

    // Check both failure points preserve the path and underlying I/O error.
    for (kind, result, expected_path) in [
        (
            "descriptor",
            open_flat_vmdk(&missing_descriptor, true),
            missing_descriptor,
        ),
        (
            "source",
            open_flat_vmdk(&descriptor_path, true),
            missing_source,
        ),
    ] {
        assert!(
            matches!(
                result,
                Err(OpenError::ResolvePath { path, source })
                    if path == expected_path && source.kind() == io::ErrorKind::NotFound
            ),
            "missing {kind} did not preserve its resolution error"
        );
    }
}

/// Checks that Unix descriptor paths can contain non-UTF-8 directory names.
#[cfg(unix)]
#[test]
fn opens_non_utf8_descriptor_path() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    // Place a regular source in a directory whose name is not valid UTF-8.
    let directory = tempfile::tempdir().unwrap();
    let non_utf8_directory_name = OsStr::from_bytes(b"vmdk-\xff");
    let source_file_name = "source.bin";
    let descriptor_file_name = "disk.vmdk";
    let parent = directory.path().join(non_utf8_directory_name);

    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join(source_file_name), [0; 512]).unwrap();

    // Keep descriptor contents valid UTF-8; only its filesystem path is non-UTF-8.
    let descriptor_path = parent.join(descriptor_file_name);
    std::fs::write(
        &descriptor_path,
        descriptor(
            "monolithicFlat",
            &format!("RW 1 FLAT \"{source_file_name}\" 0"),
        ),
    )
    .unwrap();

    // Opening must succeed without requiring a UTF-8 descriptor path.
    let handle = open_flat_vmdk(&descriptor_path, true).unwrap();
    assert_eq!(handle.files.len(), 1);
}

/// Checks that unsupported Windows path forms are rejected in descriptors and extents.
#[cfg(windows)]
#[test]
fn rejects_unsafe_windows_paths() {
    // Cover network/device paths, ambiguous roots, and alternate data streams.
    let paths = [
        (
            r"\\server\share\source.bin",
            "network paths are not supported",
        ),
        (
            r"\\?\C:\source.bin",
            "device and verbatim paths are not supported",
        ),
        (
            r"\\.\PhysicalDrive0",
            "'.' and '..' path components are not supported",
        ),
        (r"C:source.bin", "drive-relative paths are not supported"),
        (r"\source.bin", "root-relative paths are not supported"),
        (r"/source.bin", "root-relative paths are not supported"),
        (
            r"source.bin:stream",
            "alternate data streams are not supported",
        ),
    ];
    let directory = tempfile::tempdir().unwrap();

    for (path, reason) in paths {
        // Reject the path when it appears as an extent in an ordinary descriptor.
        let descriptor_path = write_descriptor(
            &directory,
            &descriptor("monolithicFlat", &format!("RW 1 FLAT \"{path}\" 0")),
        );

        assert_unsafe_path(
            open_flat_vmdk(&descriptor_path, true),
            Path::new(path),
            reason,
        );

        // Reject the same path when supplied as the descriptor path itself.
        assert_unsafe_path(
            open_flat_vmdk(Path::new(path), true),
            Path::new(path),
            reason,
        );
    }
}

/// Checks that Unix descriptor paths reject symlinks to files or parent directories.
#[cfg(unix)]
#[test]
fn rejects_symlink_descriptor() {
    use std::os::unix::fs::symlink;

    // Create a real descriptor to target with file and directory symlinks.
    let directory = tempfile::tempdir().unwrap();
    let real_directory_name = "real";
    let source_file_name = "source.bin";
    let descriptor_file_name = "disk.vmdk";
    let descriptor_link_name = "disk-link.vmdk";
    let directory_link_name = "directory-link";
    let missing_descriptor_file_name = "missing.vmdk";

    let real_directory = directory.path().join(real_directory_name);
    std::fs::create_dir(&real_directory).unwrap();

    let descriptor_path = real_directory.join(descriptor_file_name);
    std::fs::write(
        &descriptor_path,
        descriptor(
            "monolithicFlat",
            &format!("RW 1 FLAT \"{source_file_name}\" 0"),
        ),
    )
    .unwrap();

    // Link both the descriptor file and its containing directory.
    let descriptor_link = directory.path().join(descriptor_link_name);
    symlink(&descriptor_path, &descriptor_link).unwrap();

    let directory_link = directory.path().join(directory_link_name);
    symlink(&real_directory, &directory_link).unwrap();

    // A symlinked directory is rejected even when the descriptor beneath it is missing.
    for path in [
        descriptor_link,
        directory_link.join(descriptor_file_name),
        directory_link.join(missing_descriptor_file_name),
    ] {
        assert!(matches!(
            open_flat_vmdk(&path, true),
            Err(OpenError::UnsafePath { .. })
        ));
    }
}

/// Checks that Unix extent paths reject symlinked files and parent directories.
#[cfg(unix)]
#[test]
fn rejects_symlink_source() {
    use std::os::unix::fs::symlink;

    // Create source files for direct-file and parent-directory symlink cases.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    let source_directory_name = "data";
    let source_link_name = "link.bin";
    let directory_link_name = "link-data";

    let source_path = directory.path().join(source_file_name);
    let source_directory = directory.path().join(source_directory_name);
    std::fs::write(&source_path, [0; 512]).unwrap();
    std::fs::create_dir(&source_directory).unwrap();
    std::fs::write(source_directory.join(source_file_name), [0; 512]).unwrap();

    // Provide a symlink to a source file and another to a source directory.
    symlink(&source_path, directory.path().join(source_link_name)).unwrap();
    symlink(
        &source_directory,
        directory.path().join(directory_link_name),
    )
    .unwrap();

    // Reject both link types in relative and absolute extent paths.
    for source in [
        PathBuf::from(source_link_name),
        PathBuf::from(directory_link_name).join(source_file_name),
        directory.path().join(source_link_name),
        directory
            .path()
            .join(directory_link_name)
            .join(source_file_name),
    ] {
        let path = write_descriptor(
            &directory,
            &descriptor(
                "monolithicFlat",
                &format!("RW 1 FLAT \"{}\" 0", source.display()),
            ),
        );

        assert!(matches!(
            open_flat_vmdk(&path, true),
            Err(OpenError::UnsafePath { .. })
        ));
    }
}

/// Checks that Unix device files cannot be used as extent sources.
#[cfg(unix)]
#[test]
fn rejects_non_regular_source() {
    // /dev/null exists but is a device, not a regular file.
    let directory = tempfile::tempdir().unwrap();
    let non_regular_source = "/dev/null";

    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 1 FLAT \"{non_regular_source}\" 0"),
        ),
    );

    assert!(matches!(
        open_flat_vmdk(&path, true),
        Err(OpenError::ExtentNotRegularFile(0))
    ));
}

/// Checks that the descriptor-size limit is accepted and one extra byte is rejected.
#[test]
fn enforces_descriptor_size_limit() {
    // Create a valid descriptor and source before extending the descriptor.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    let descriptor_file_name = "disk.vmdk";

    std::fs::write(directory.path().join(source_file_name), [0; 512]).unwrap();
    let path = directory.path().join(descriptor_file_name);
    let mut contents = descriptor(
        "monolithicFlat",
        &format!("RW 1 FLAT \"{source_file_name}\" 0"),
    )
    .into_bytes();

    // Pad to exactly the limit using a comment that does not change the extent map.
    contents.resize(MAX_DESCRIPTOR_SIZE as usize, b'#');
    std::fs::write(&path, &contents).unwrap();

    // A descriptor at the limit must still open.
    open_flat_vmdk(&path, true).unwrap();

    // One additional byte must fail the size check.
    contents.push(b'#');
    std::fs::write(&path, contents).unwrap();

    assert!(matches!(
        open_flat_vmdk(&path, true),
        Err(OpenError::DescriptorTooLarge)
    ));
}

/// Checks that an extent cannot claim more sectors than its source file provides.
#[test]
fn rejects_source_range_past_end() {
    // Provide one sector of source data but declare a two-sector extent.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    std::fs::write(directory.path().join(source_file_name), [0; 512]).unwrap();

    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 2 FLAT \"{source_file_name}\" 0"),
        ),
    );

    assert!(matches!(
        open_flat_vmdk(&path, true),
        Err(OpenError::InvalidExtentMap(_))
    ));
}

/// Checks that extent reads honor descriptor order, source offsets, and spaced paths.
///
/// Both `RW` and `RDONLY` descriptor entries are readable through this backend.
#[async_test]
async fn reads_across_ordered_extents() {
    // Give each source sector a distinct value to expose ordering or offset mistakes.
    let directory = tempfile::tempdir().unwrap();
    let first_source_file_name = "first.bin";
    let second_source_file_name = "second file.bin";
    let first = directory.path().join(first_source_file_name);
    let second = directory.path().join(second_source_file_name);

    std::fs::write(&first, [[0x10; 512], [0x11; 512]].concat()).unwrap();
    std::fs::write(&second, [[0x20; 512], [0x21; 512], [0x22; 512]].concat()).unwrap();

    // Skip the first source's leading sector, then append two sectors from the second.
    let path = write_descriptor(
        &directory,
        &descriptor(
            "twoGbMaxExtentFlat",
            &format!(
                "RW 1 FLAT \"{first_source_file_name}\" 1\nRDONLY 2 FLAT \"{second_source_file_name}\""
            ),
        ),
    );
    let disk = open_disk(&path);

    // The virtual disk must concatenate only the declared source ranges.
    assert_eq!(disk.sector_count(), 3);
    assert_eq!(
        read_sectors(&disk, 0, 3).await,
        [[0x11; 512], [0x20; 512], [0x21; 512]].concat()
    );
}

/// Checks that 513 extents reuse one source handle and honor per-extent offsets.
#[async_test]
async fn reuses_source_file_across_more_than_512_extents() {
    // Make source offsets distinguishable by storing a different byte in each sector.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    std::fs::write(
        directory.path().join(source_file_name),
        [[0x30; 512], [0x31; 512], [0x32; 512]].concat(),
    )
    .unwrap();

    // Alternate offsets 2 and 0, then add one extent to exceed 512 entries.
    let extent_pair =
        format!("RW 1 FLAT \"{source_file_name}\" 2\nRW 1 FLAT \"{source_file_name}\" 0\n");
    let mut extents = extent_pair.repeat(256);
    extents.push_str(&format!("RW 1 FLAT \"{source_file_name}\" 2\n"));
    let path = write_descriptor(&directory, &descriptor("twoGbMaxExtentFlat", &extents));

    // Repeated paths must share one source handle without collapsing extent entries.
    let handle = open_flat_vmdk(&path, true).unwrap();
    assert_eq!(handle.files.len(), 1);
    assert_eq!(handle.extents.len(), 513);

    // Read across the first extent boundary and check both source offsets.
    let disk = Disk::new(FlatVmdkDisk::new(handle).unwrap()).unwrap();
    assert_eq!(disk.sector_count(), 513);
    assert_eq!(
        read_sectors(&disk, 0, 2).await,
        [[0x32; 512], [0x30; 512]].concat()
    );
}

/// Checks request bounds, empty reads, and read plans at extent boundaries.
///
/// Also checks that cloned source handles remain usable after the disk is dropped.
#[async_test]
async fn plans_reads_at_and_across_extent_boundaries() {
    // Build a four-sector disk with a source-file boundary at virtual sector 2.
    let directory = tempfile::tempdir().unwrap();
    let first_source_file_name = "first.bin";
    let second_source_file_name = "second.bin";

    std::fs::write(directory.path().join(first_source_file_name), [0; 1024]).unwrap();
    std::fs::write(directory.path().join(second_source_file_name), [0; 1024]).unwrap();

    let path = write_descriptor(
        &directory,
        &descriptor(
            "twoGbMaxExtentFlat",
            &format!(
                "RW 2 FLAT \"{first_source_file_name}\" 0\nRW 2 FLAT \"{second_source_file_name}\" 0"
            ),
        ),
    );

    let disk = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();

    // Reject a byte count that is not a whole sector.
    let memory = GuestMemory::allocate(512);
    assert!(matches!(
        disk.read_vectored(&OwnedRequestBuffers::linear(0, 1, true).buffer(&memory), 0,)
            .await,
        Err(DiskError::InvalidInput)
    ));

    // Reject reads starting beyond the disk or overflowing the sector range.
    for sector in [disk.sector_count(), u64::MAX] {
        assert!(matches!(
            disk.read_vectored(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&memory),
                sector,
            )
            .await,
            Err(DiskError::IllegalBlock)
        ));
    }

    // An empty read at the disk's end is valid and requires no segments.
    disk.read_vectored(
        &OwnedRequestBuffers::linear(0, 0, true).buffer(&memory),
        disk.sector_count(),
    )
    .await
    .unwrap();
    assert!(
        disk.plan_read(disk.sector_count(), disk.sector_count(), 1)
            .is_empty()
    );

    // A read spanning sectors 1 and 2 must split between the two sources.
    let segments = disk.plan_read(1, 3, 0);
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].source_index, 0);
    assert_eq!(segments[0].source_byte_offset, SECTOR_SIZE);
    assert_eq!(segments[0].byte_count, SECTOR_SIZE as usize);
    assert_eq!(segments[1].source_index, 1);
    assert_eq!(segments[1].source_byte_offset, 0);
    assert_eq!(segments[1].byte_count, SECTOR_SIZE as usize);

    // A read starting exactly at the boundary must use only the second source.
    let segments = disk.plan_read(2, 3, 1);
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].source_index, 1);
    assert_eq!(segments[0].source_byte_offset, 0);

    // The worker's shared table must keep source handles alive after disk teardown.
    let sources = disk.sources.clone();
    drop(disk);
    let mut output = [0xa5; 512];
    FlatVmdkDisk::read_segments_blocking(&sources, &mut output, segments).unwrap();
    assert_eq!(output, [0; 512]);
}

/// Checks that blocking read plans fill, but never exceed, the output buffer.
///
/// Invalid plans must report an error without changing the sentinel bytes.
#[test]
fn rejects_incomplete_fallback_read_plan() {
    // An empty plan cannot fill a nonempty destination.
    let mut request_output = [0xa5; 512];
    let error =
        FlatVmdkDisk::read_segments_blocking(&[], &mut request_output, Vec::new()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "VMDK read segments do not fill the request buffer"
    );

    // A segment larger than the destination must fail before reading or padding.
    let sources = [Source {
        file: Arc::new(tempfile::tempfile().unwrap()),
        length: 0,
    }];
    let segments = vec![ReadSegment {
        source_index: 0,
        source_byte_offset: 0,
        byte_count: request_output.len() + 1,
    }];

    let error =
        FlatVmdkDisk::read_segments_blocking(&sources, &mut request_output, segments).unwrap_err();
    assert_eq!(
        error.to_string(),
        "VMDK read segment exceeds request buffer"
    );
    assert_eq!(request_output, [0xa5; 512]);
}

/// Checks sector sizes, rejected writes, and successful read-only flush/unmap calls.
#[async_test]
async fn read_only_protocol_behavior() {
    // Open a two-sector disk through the public disk interface.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    std::fs::write(directory.path().join(source_file_name), [0; 1024]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 2 FLAT \"{source_file_name}\" 0"),
        ),
    );
    let disk = open_disk(&path);
    let memory = GuestMemory::allocate(512);

    // Both logical and physical sector sizes are reported as 512 bytes.
    assert_eq!(disk.sector_size(), 512);
    assert_eq!(disk.physical_sector_size(), 512);

    // A writable descriptor entry does not make the backend writable.
    let result = disk
        .write_vectored(
            &OwnedRequestBuffers::linear(0, 512, false).buffer(&memory),
            0,
            false,
        )
        .await;
    assert!(matches!(result, Err(DiskError::ReadOnly)));

    // Flush and unmap are accepted as no-ops on this read-only backend.
    disk.sync_cache().await.unwrap();
    disk.unmap(0, 1, false).await.unwrap();
}

/// Runs the shared disk range checks, including zero-length and out-of-bounds reads.
#[async_test]
async fn sector_range_conformance() {
    // Supply a small valid disk to the common storage conformance tests.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    std::fs::write(directory.path().join(source_file_name), [0; 1024]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 2 FLAT \"{source_file_name}\" 0"),
        ),
    );

    storage_tests::sector_range::test_disk_sector_range_conformance(&open_disk(&path)).await;
}

/// Checks that blocking reads above the cache limit succeed without retaining a buffer.
#[async_test]
async fn does_not_cache_oversized_reads() {
    // Make the request one sector larger than the largest cacheable buffer.
    let directory = tempfile::tempdir().unwrap();
    let byte_count = MAX_CACHED_READ_BUFFER_SIZE + SECTOR_SIZE as usize;
    std::fs::write(directory.path().join("source.bin"), vec![0x5a; byte_count]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!(
                "RW {} FLAT \"source.bin\" 0",
                byte_count / SECTOR_SIZE as usize
            ),
        ),
    );

    // Leave native I/O disabled so this request exercises the blocking buffer.
    let disk = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();

    // The entire request must still be read correctly.
    assert_eq!(
        read_backend_sectors(&disk, 0, byte_count / SECTOR_SIZE as usize)
            .await
            .unwrap(),
        vec![0x5a; byte_count]
    );

    // Oversized temporary allocations must not enter the cache.
    assert!(disk.read_buffer.lock().is_empty());
}

/// Checks final-sector zero padding with both a fresh and a reused blocking buffer.
///
/// The second read reuses an allocation containing nonzero bytes from the first.
#[async_test]
async fn reuses_read_buffer_and_clears_partial_sector_padding() {
    // Store a full nonzero sector followed by one byte of a second sector.
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.bin");
    let mut data = vec![0xa5; 512];
    data.push(0x41);
    std::fs::write(&source, data).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor(
            "monolithicFlat",
            &format!("RW 2 FLAT \"{}\" 0", source.display()),
        ),
    );
    let disk = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();

    // The first read must preserve file bytes and zero the incomplete sector's tail.
    let data = read_backend_sectors(&disk, 0, 2).await.unwrap();
    assert_eq!(&data[..512], &[0xa5; 512]);
    assert_eq!(data[512], 0x41);
    assert!(data[513..].iter().all(|byte| *byte == 0));
    let pointer = disk.read_buffer.lock().as_ptr();

    // Reuse the allocation for a shorter read; old data must not appear as padding.
    let data = read_backend_sectors(&disk, 1, 1).await.unwrap();
    assert_eq!(disk.read_buffer.lock().as_ptr(), pointer);
    assert_eq!(disk.read_buffer.lock().len(), 512);
    assert_eq!(data[0], 0x41);
    assert!(data[1..].iter().all(|byte| *byte == 0));
}

/// Checks that a failed blocking read returns its buffer to the cache for reuse.
///
/// Truncating an opened source must produce an error, not final-sector padding.
#[async_test]
async fn failed_read_returns_buffer_to_cache() {
    // Open a source with two complete sectors so both are expected to exist.
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.bin");
    std::fs::write(&source, [0x50; 1024]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor("monolithicFlat", "RW 2 FLAT \"source.bin\" 0"),
    );
    let disk = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();

    // Warm the buffer cache and remember its allocation.
    assert_eq!(
        read_backend_sectors(&disk, 0, 2).await.unwrap(),
        [0x50; 1024]
    );
    let pointer = disk.read_buffer.lock().as_ptr();

    // Remove the second sector after the backend has cached the original file size.
    OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_len(512)
        .unwrap();

    // The missing sector must fail, but the buffer must still return to the cache.
    let result = read_backend_sectors(&disk, 1, 1).await;
    assert!(
        matches!(result, Err(DiskError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
    );
    assert_eq!(disk.read_buffer.lock().as_ptr(), pointer);

    // A subsequent valid read must succeed using the same allocation.
    assert_eq!(
        read_backend_sectors(&disk, 0, 1).await.unwrap(),
        [0x50; 512]
    );
    assert_eq!(disk.read_buffer.lock().as_ptr(), pointer);
    assert_eq!(disk.read_buffer.lock().len(), 512);
}

/// Checks that a guest-memory error in one native segment does not cancel another.
///
/// Each segment is made invalid in turn; the valid sibling must finish its read.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_read_errors_wait_for_pending_siblings(driver: pal_async::DefaultDriver) {
    // Map two virtual extents to the same eight-sector source.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.bin"), [0xa5; 4096]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor(
            "twoGbMaxExtentFlat",
            "RW 8 FLAT \"source.bin\" 0\nRW 8 FLAT \"source.bin\" 0",
        ),
    );

    // Enable native reads so each extent writes directly into its guest pages.
    let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
    backend.enable_native_io(driver).await;
    assert_eq!(backend.native_sources.len(), 1);

    // Only page 0 exists; fail each read in turn by targeting page 1.
    for pages in [[1, 0], [0, 1]] {
        let memory = GuestMemory::allocate(4096);
        let buffers = OwnedRequestBuffers::new_unaligned(&pages, 0, 8192);
        let result = backend.read_vectored(&buffers.buffer(&memory), 0).await;
        assert!(matches!(result, Err(DiskError::MemoryAccess(_))));

        // The valid read must finish despite the other segment's lock error.
        let mut actual = [0; 4096];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, [0xa5; 4096]);
    }
}

/// Checks that a native read future fits the disk interface's inline storage budget.
///
/// The read is also completed to verify that measuring it does not replace execution.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_read_future_fits_inline_storage(driver: pal_async::DefaultDriver) {
    // Prepare a one-sector source and enable native reads.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.bin"), [0x91; 512]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor("monolithicFlat", "RW 1 FLAT \"source.bin\" 0"),
    );
    let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
    backend.enable_native_io(driver).await;

    // Construct the future without polling it so its stored size can be measured.
    let memory = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    let buffers = owned.buffer(&memory);
    let read = backend.read_vectored(&buffers, 0);

    // ASYNC_DISK_STACK_SIZE in disk_backend reserves 1256 bytes per operation.
    assert!(
        size_of_val(&read) <= 1256,
        "native read future needs {} bytes",
        size_of_val(&read)
    );

    // Complete the measured future and verify its destination bytes.
    read.await.unwrap();

    let mut actual = [0; 512];
    memory.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0x91; 512]);
}

/// Checks small and large native reads across complete and partial-sector sources.
///
/// Reused guest buffers must receive zero padding, and later source truncation
/// must fail on both single-extent and multi-extent reads.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_io_reads_partial_sectors(driver: pal_async::DefaultDriver) {
    // Exercise both a small request and one larger than 1 MiB.
    for full_sector_count in [2usize, 4096] {
        // Give the first source a distinct leading sector and the second only one byte.
        let directory = tempfile::tempdir().unwrap();
        let byte_count = full_sector_count * 512;
        let mut full = vec![0xa5; byte_count];
        full[..512].fill(0x11);
        std::fs::write(directory.path().join("full.bin"), &full).unwrap();
        std::fs::write(directory.path().join("partial.bin"), [0x41]).unwrap();

        // Append the partial source as one virtual sector after the complete source.
        let path = write_descriptor(
            &directory,
            &descriptor(
                "twoGbMaxExtentFlat",
                &format!("RW {full_sector_count} FLAT \"full.bin\" 0\nRW 1 FLAT \"partial.bin\" 0"),
            ),
        );

        // Prepare native readers for both files.
        let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
        backend.enable_native_io(driver.clone()).await;
        assert_eq!(backend.native_sources.len(), 2);

        // Cover the single-extent fast path, including reads larger than 1 MiB.
        assert_eq!(
            read_backend_sectors(&backend, 0, full_sector_count)
                .await
                .unwrap(),
            full
        );

        // Span both extents and ensure reused guest buffers are zero-padded.
        let memory = GuestMemory::allocate(byte_count);
        let owned = OwnedRequestBuffers::linear(0, byte_count, true);
        let partial_start = byte_count - 512;
        let mut expected = vec![0xa5; byte_count];
        expected[partial_start] = 0x41;
        expected[partial_start + 1..].fill(0);

        // Dirty the destination before each read to expose missing zero padding.
        for _ in 0..2 {
            memory.write_at(0, &vec![0xff; byte_count]).unwrap();
            backend
                .read_vectored(&owned.buffer(&memory), 1)
                .await
                .unwrap();

            let mut data = vec![0; byte_count];
            memory.read_at(0, &mut data).unwrap();
            assert_eq!(data, expected);
        }

        // Successful native reads must not populate the blocking buffer cache.
        assert!(backend.read_buffer.lock().is_empty());

        // Padding is based on the cached length, not on a later short host read.
        OpenOptions::new()
            .write(true)
            .open(directory.path().join("partial.bin"))
            .unwrap()
            .set_len(0)
            .unwrap();

        // A read of the partial extent alone must detect its missing file byte.
        let result = read_backend_sectors(&backend, full_sector_count as u64, 1).await;
        assert!(
            matches!(result, Err(DiskError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );

        // A read spanning both extents must report the same truncation error.
        let result = backend.read_vectored(&owned.buffer(&memory), 1).await;
        assert!(
            matches!(result, Err(DiskError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
}

/// Checks native padding for every final-sector length and a nonzero source offset.
///
/// Replacing the source path must not change the already-open file used for reads.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_io_reads_all_final_sector_lengths(driver: pal_async::DefaultDriver) {
    // Map the virtual sector to the second sector of an opened source file.
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.bin");
    std::fs::write(&source, [0x41; 1024]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor("monolithicFlat", "RW 1 FLAT \"source.bin\" 1"),
    );
    let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
    backend.enable_native_io(driver).await;
    let native_source = &backend.native_sources[0];

    // Replace the path with different data while retaining the original open file.
    std::fs::rename(&source, directory.path().join("previous.bin")).unwrap();
    std::fs::write(&source, [0x99; 1024]).unwrap();

    // Reads must still return bytes from the original file, not the replacement.
    let memory = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    backend
        .read_vectored(&owned.buffer(&memory), 0)
        .await
        .unwrap();

    let mut data = [0; 512];
    memory.read_at(0, &mut data).unwrap();
    assert_eq!(data, [0x41; 512]);
    assert!(backend.read_buffer.lock().is_empty());

    // Vary the cached length to cover every padding size with one open source.
    for final_byte_count in 1..=512 {
        // Use nonzero destination bytes so any missing zero padding is visible.
        memory.write_at(0, &[0xff; 512]).unwrap();

        // Simulate a file ending final_byte_count bytes into its second sector.
        FlatVmdkDisk::read_native_range(
            native_source,
            owned.buffer(&memory),
            (512 + final_byte_count) as u64,
            512,
        )
        .await
        .unwrap();

        // Preserve the declared file bytes and zero only the rest of the sector.
        memory.read_at(0, &mut data).unwrap();
        assert!(
            data[..final_byte_count].iter().all(|byte| *byte == 0x41),
            "final sector length {final_byte_count}"
        );
        assert!(
            data[final_byte_count..].iter().all(|byte| *byte == 0),
            "final sector length {final_byte_count}"
        );
    }
}

/// Checks that a read spanning 513 extents uses native I/O without a blocking buffer.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_io_reads_many_extents_without_fallback(driver: pal_async::DefaultDriver) {
    // Reuse one source sector across enough extents to exercise a large read plan.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.bin"), [0xa5; 512]).unwrap();
    let count = 513;
    let path = write_descriptor(
        &directory,
        &descriptor(
            "twoGbMaxExtentFlat",
            &"RW 1 FLAT \"source.bin\" 0\n".repeat(count),
        ),
    );

    // Enable native I/O and read all extents in one guest request.
    let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
    backend.enable_native_io(driver).await;

    assert_eq!(
        read_backend_sectors(&backend, 0, count).await.unwrap(),
        vec![0xa5; count * 512]
    );

    // A blocking fallback would have populated the reusable buffer cache.
    assert!(backend.read_buffer.lock().is_empty());
}

/// Checks concurrent native reads, destination permissions, and unaligned addresses.
///
/// Guest addresses need not be sector-aligned, but request lengths must contain
/// whole sectors. Rejected read-only destinations must remain unchanged.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_io_handles_concurrent_reads_and_read_only_buffers(
    driver: pal_async::DefaultDriver,
) {
    // Give each sector distinct data to distinguish concurrent read results.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("source.bin"),
        [[0x11; 512], [0x22; 512]].concat(),
    )
    .unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor("monolithicFlat", "RW 2 FLAT \"source.bin\" 0"),
    );

    let mut backend = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap()).unwrap();
    backend.enable_native_io(driver).await;

    // Read different sectors concurrently through the same backend.
    let (first, second) = futures::join!(
        read_backend_sectors(&backend, 0, 1),
        read_backend_sectors(&backend, 1, 1),
    );
    assert_eq!(first.unwrap(), [0x11; 512]);
    assert_eq!(second.unwrap(), [0x22; 512]);

    // A read-only destination must reject the read without changing its bytes.
    let memory = GuestMemory::allocate(512);
    memory.write_at(0, &[0x99; 512]).unwrap();
    let result = backend
        .read_vectored(
            &OwnedRequestBuffers::linear(0, 512, false).buffer(&memory),
            0,
        )
        .await;
    assert!(result.is_err());

    let mut unchanged = [0; 512];
    memory.read_at(0, &mut unchanged).unwrap();
    assert_eq!(unchanged, [0x99; 512]);

    // Read both sectors into a destination starting three bytes into a guest page.
    let memory = GuestMemory::allocate(4096);
    backend
        .read_vectored(
            &OwnedRequestBuffers::new_unaligned(&[0], 3, 1024).buffer(&memory),
            0,
        )
        .await
        .unwrap();

    let mut unaligned = [0; 1024];
    memory.read_at(3, &mut unaligned).unwrap();
    assert_eq!(unaligned, [[0x11; 512], [0x22; 512]].concat().as_slice());

    // The unaligned destination must still use native I/O, not the blocking cache.
    assert!(backend.read_buffer.lock().is_empty());

    // Unaligned addresses are allowed, but the length must still be whole sectors.
    assert!(matches!(
        backend
            .read_vectored(
                &OwnedRequestBuffers::linear(0, 513, true).buffer(&memory),
                0,
            )
            .await,
        Err(DiskError::InvalidInput)
    ));
}
