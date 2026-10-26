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

    Disk::new(
        FlatVmdkDisk::new(
            handle,
            #[cfg(target_os = "linux")]
            None,
        )
        .unwrap(),
    )
    .unwrap()
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

/// Checks that extent paths resolve against the descriptor's directory.
///
/// Relative descriptor paths (including `.` components) and extent paths
/// containing `..` must both resolve, matching other disk backends.
#[async_test]
async fn resolves_relative_descriptor_and_extent_paths() {
    // Create the fixture beneath the current directory so the descriptor
    // can be opened using either an absolute or relative path.
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir_in(&cwd).unwrap();

    // Keep the descriptor and source in sibling directories.
    let descriptor_directory = directory.path().join("descriptors");
    let source_directory = directory.path().join("images.with.dots");
    std::fs::create_dir(&descriptor_directory).unwrap();
    std::fs::create_dir(&source_directory).unwrap();
    std::fs::write(source_directory.join(".source.bin"), [0x25; 512]).unwrap();

    // The relative extent path is resolved from the descriptor directory.
    let descriptor_path = descriptor_directory.join("disk.vmdk");
    std::fs::write(
        &descriptor_path,
        descriptor(
            "monolithicFlat",
            "RW 1 FLAT \"../images.with.dots/.source.bin\" 0",
        ),
    )
    .unwrap();

    // Absolute, relative, and `.`-prefixed descriptor paths must all resolve
    // to the same source data.
    let relative_descriptor_path = descriptor_path.strip_prefix(&cwd).unwrap();
    for descriptor_path in [
        descriptor_path.clone(),
        relative_descriptor_path.to_owned(),
        Path::new(".").join(relative_descriptor_path),
    ] {
        let disk = open_disk(&descriptor_path);
        assert_eq!(read_sectors(&disk, 0, 1).await, [0x25; 512]);
    }
}

/// Checks that missing descriptors and sources report their paths and `NotFound`.
#[test]
fn open_errors_preserve_missing_paths() {
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

    // Source errors report the path joined to the descriptor directory.
    let missing_source = directory.path().join(missing_source_file_name);

    assert!(
        matches!(
            open_flat_vmdk(&missing_descriptor, true),
            Err(OpenError::OpenDescriptor { path, source })
                if path == missing_descriptor && source.kind() == io::ErrorKind::NotFound
        ),
        "missing descriptor did not preserve its open error"
    );
    assert!(
        matches!(
            open_flat_vmdk(&descriptor_path, true),
            Err(OpenError::OpenExtent { path, source })
                if path == missing_source && source.kind() == io::ErrorKind::NotFound
        ),
        "missing source did not preserve its open error"
    );
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

/// Checks that symbolic links in descriptor and extent paths are followed.
///
/// Relative extents resolve against the directory of the descriptor path as
/// given, not the link target's directory, matching VHDX parent locators.
#[cfg(unix)]
#[async_test]
async fn follows_symlinks() {
    use std::os::unix::fs::symlink;

    // Place the descriptor and source in a real directory.
    let directory = tempfile::tempdir().unwrap();
    let real_directory = directory.path().join("real");
    std::fs::create_dir(&real_directory).unwrap();
    std::fs::write(real_directory.join("source.bin"), [0x5c; 512]).unwrap();
    std::fs::write(
        real_directory.join("disk.vmdk"),
        descriptor("monolithicFlat", "RW 1 FLAT \"source-link.bin\" 0"),
    )
    .unwrap();

    // Provide the linked source name next to the real descriptor and next to
    // the descriptor link.
    for link_directory in [real_directory.as_path(), directory.path()] {
        symlink(
            real_directory.join("source.bin"),
            link_directory.join("source-link.bin"),
        )
        .unwrap();
    }

    // Reach the descriptor through both a directory link and a file link.
    let directory_link = directory.path().join("linked");
    symlink(&real_directory, &directory_link).unwrap();
    let descriptor_link = directory.path().join("disk-link.vmdk");
    symlink(real_directory.join("disk.vmdk"), &descriptor_link).unwrap();

    for path in [directory_link.join("disk.vmdk"), descriptor_link] {
        let disk = open_disk(&path);
        assert_eq!(read_sectors(&disk, 0, 1).await, [0x5c; 512]);
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

/// Checks that layout validation rejects empty extents and extents that end
/// past their source file.
#[test]
fn rejects_invalid_extent_ranges() {
    // Provide one sector of source data.
    let directory = tempfile::tempdir().unwrap();
    let source_file_name = "source.bin";
    std::fs::write(directory.path().join(source_file_name), [0; 512]).unwrap();

    for (sector_count, expected) in [
        (0, "extent 0 has zero sectors"),
        (2, "extent 0 ends at source sector 2"),
    ] {
        let path = write_descriptor(
            &directory,
            &descriptor(
                "monolithicFlat",
                &format!("RW {sector_count} FLAT \"{source_file_name}\" 0"),
            ),
        );

        assert!(matches!(
            open_flat_vmdk(&path, true),
            Err(OpenError::InvalidExtentMap(message)) if message.contains(expected)
        ));
    }
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
    let disk = Disk::new(
        FlatVmdkDisk::new(
            handle,
            #[cfg(target_os = "linux")]
            None,
        )
        .unwrap(),
    )
    .unwrap();
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

    let disk = FlatVmdkDisk::new(
        open_flat_vmdk(&path, true).unwrap(),
        #[cfg(target_os = "linux")]
        None,
    )
    .unwrap();

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
    let disk = FlatVmdkDisk::new(
        open_flat_vmdk(&path, true).unwrap(),
        #[cfg(target_os = "linux")]
        None,
    )
    .unwrap();

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
    let disk = FlatVmdkDisk::new(
        open_flat_vmdk(&path, true).unwrap(),
        #[cfg(target_os = "linux")]
        None,
    )
    .unwrap();

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
    let disk = FlatVmdkDisk::new(
        open_flat_vmdk(&path, true).unwrap(),
        #[cfg(target_os = "linux")]
        None,
    )
    .unwrap();

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

#[cfg(target_os = "linux")]
#[async_test]
async fn unsupported_driver_uses_blocking_reads(driver: pal_async::DefaultDriver) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.bin"), [0xa5; 512]).unwrap();
    let path = write_descriptor(
        &directory,
        &descriptor("monolithicFlat", "RW 1 FLAT \"source.bin\" 0"),
    );

    for opcode in [
        ::io_uring::opcode::Read::CODE,
        ::io_uring::opcode::Readv::CODE,
    ] {
        let driver = crate::io_uring::tests::InterruptedDriver {
            inner: driver.clone(),
            interruptions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            unsupported_opcode: Some(opcode),
            in_flight: None,
        };
        let disk = FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver)))
            .unwrap();

        assert!(disk.driver.is_none());
        assert_eq!(
            read_backend_sectors(&disk, 0, 1).await.unwrap(),
            [0xa5; 512]
        );
        assert_eq!(disk.read_buffer.lock().len(), 512);
    }
}

/// Checks that native reads stop after the first failed segment.
///
/// Each segment is made invalid in turn. An earlier valid segment still
/// completes, but no segment after a failure is read.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_read_errors_stop_later_reads(driver: pal_async::DefaultDriver) {
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
    let backend =
        FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver))).unwrap();
    assert!(backend.driver.is_some());

    // Only page 0 exists; fail each read in turn by targeting page 1.
    for pages in [[1, 0], [0, 1]] {
        let memory = GuestMemory::allocate(4096);
        let buffers = OwnedRequestBuffers::new_unaligned(&pages, 0, 8192);
        let result = backend.read_vectored(&buffers.buffer(&memory), 0).await;
        assert!(matches!(result, Err(DiskError::MemoryAccess(_))));

        // A valid segment before the failure fills page 0; a valid segment
        // after the failure is never read, leaving page 0 zeroed.
        let expected = if pages[0] == 0 {
            [0xa5; 4096]
        } else {
            [0; 4096]
        };
        let mut actual = [0; 4096];
        memory.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, expected);
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
    let backend =
        FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver))).unwrap();

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

        // Prepare one native driver for both files.
        let backend = FlatVmdkDisk::new(
            open_flat_vmdk(&path, true).unwrap(),
            Some(Box::new(driver.clone())),
        )
        .unwrap();
        assert!(backend.driver.is_some());

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
    let backend =
        FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver))).unwrap();
    let driver = backend.driver.as_deref().unwrap();

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
        let source = Source {
            file: backend.sources[0].file.clone(),
            length: (512 + final_byte_count) as u64,
        };
        FlatVmdkDisk::read_native_range(driver, &source, owned.buffer(&memory), 512)
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

/// Checks that a read spanning 513 extents uses native I/O without a blocking
/// buffer and stays within the subread limit.
#[cfg(target_os = "linux")]
#[async_test]
async fn native_io_reads_many_extents_without_fallback(driver: pal_async::DefaultDriver) {
    // A zero limit would never start a subread, so reads would hang.
    assert_ne!(MAX_READ_SUB_WORKERS, 0);

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

    // Enable native I/O, counting subreads in flight, and read all extents in
    // one guest request.
    let in_flight = Arc::new(crate::io_uring::tests::InFlight::default());
    let driver = crate::io_uring::tests::InterruptedDriver {
        inner: driver,
        interruptions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        unsupported_opcode: None,
        in_flight: Some(in_flight.clone()),
    };
    let backend =
        FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver))).unwrap();

    assert_eq!(
        read_backend_sectors(&backend, 0, count).await.unwrap(),
        vec![0xa5; count * 512]
    );

    // The request kept exactly the limit of subreads in flight.
    assert_eq!(in_flight.peak.load(Ordering::SeqCst), MAX_READ_SUB_WORKERS);

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

    let backend =
        FlatVmdkDisk::new(open_flat_vmdk(&path, true).unwrap(), Some(Box::new(driver))).unwrap();

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
