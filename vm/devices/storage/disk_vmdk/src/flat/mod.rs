// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Opens flat VMDK disks and implements their read-only disk backend.
//!
//! The opener parses a descriptor, resolves its extent files, and constructs
//! the resource handle transported to the VM worker. The backend revalidates
//! that handle, maps virtual sectors across the ordered extents, and serves
//! disk reads.

mod descriptor;
#[cfg(test)]
mod tests;

use crate::MAX_DESCRIPTOR_SIZE;
use crate::OpenError;
use async_trait::async_trait;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::FlatVmdkDiskHandle;
use disk_backend_resources::FlatVmdkExtent;
use disk_backend_resources::VmdkExtentAccess;
#[cfg(target_os = "linux")]
use futures::StreamExt;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;

// Flat VMDK descriptors measure extent sizes and offsets in 512-byte sectors.
const SECTOR_SIZE: u64 = 512;
// Physical sector size in bytes reported by this backend.
const DEFAULT_PHYSICAL_SECTOR_SIZE: u32 = 512;
// Maximum number of 512-byte sectors that fit within i64::MAX bytes.
const MAX_SECTOR_COUNT: u64 = (i64::MAX as u64) / SECTOR_SIZE;
// Maximum temporary-buffer capacity retained for reuse after blocking reads.
const MAX_CACHED_READ_BUFFER_SIZE: usize = 1024 * 1024;
// Maximum concurrent native subreads per guest request.
#[cfg(target_os = "linux")]
const MAX_READ_SUB_WORKERS: usize = 1;

/// Converts transported [`FlatVmdkDiskHandle`] resources into read-only disks.
///
/// The resolver revalidates every source handle and extent before exposing the
/// disk in the VM worker.
pub struct FlatVmdkResolver;
declare_static_async_resolver!(FlatVmdkResolver, (DiskHandleKind, FlatVmdkDiskHandle));

/// Errors that can occur when opening a flat VMDK resource as a disk.
#[derive(Debug, Error)]
pub enum ResolveFlatVmdkError {
    /// Write access was requested, but VMDK disks are read-only.
    #[error("VMDK disks are read-only")]
    ReadWriteUnsupported,
    /// The transported files or extent map failed VMDK validation.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// The constructed disk geometry is invalid.
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, FlatVmdkDiskHandle> for FlatVmdkResolver {
    type Output = ResolvedDisk;
    type Error = ResolveFlatVmdkError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: FlatVmdkDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Flat VMDK disks do not expose writable access.
        if !input.read_only {
            return Err(ResolveFlatVmdkError::ReadWriteUnsupported);
        }

        // Revalidate the transported handle before wrapping the backend.
        let disk = FlatVmdkDisk::new(
            resource,
            #[cfg(target_os = "linux")]
            Some(Box::new(input.driver_source.current())),
        )?;
        ResolvedDisk::new(disk).map_err(ResolveFlatVmdkError::InvalidDisk)
    }
}

/// Opens and validates a flat VMDK descriptor and its extent files.
///
/// Returns a [`FlatVmdkDiskHandle`] containing the opened source files and
/// validated ordered extent map for transport to the VM worker.
pub(crate) fn open_flat_vmdk(
    path: &Path,
    read_only: bool,
) -> Result<FlatVmdkDiskHandle, OpenError> {
    // Presently, we do not support write access to flat VMDK disks.
    if !read_only {
        return Err(OpenError::ReadWriteUnsupported);
    }

    // Open the descriptor file.
    let descriptor = File::open(path).map_err(|source| OpenError::OpenDescriptor {
        path: path.to_owned(),
        source,
    })?;

    // Ensure that the descriptor is a regular file before reading it.
    if !descriptor
        .metadata()
        .map_err(|source| OpenError::ReadDescriptorMetadata {
            path: path.to_owned(),
            source,
        })?
        .is_file()
    {
        return Err(OpenError::DescriptorNotRegularFile(path.to_owned()));
    }

    // Read one byte beyond the limit to detect a descriptor that is too large.
    let mut bytes = Vec::new();
    descriptor
        .take(MAX_DESCRIPTOR_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| OpenError::ReadDescriptor {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_DESCRIPTOR_SIZE {
        return Err(OpenError::DescriptorTooLarge);
    }

    // Parse the descriptor to extract the extent entries.
    let parsed_extents = descriptor::parse_descriptor(&bytes)?;

    // Relative extent paths are resolved against the descriptor's directory;
    // absolute extent paths replace it.
    let descriptor_directory = path.parent().unwrap_or(Path::new("."));

    let mut file_indices = HashMap::new();
    let mut files = Vec::new();
    let mut extents = Vec::with_capacity(parsed_extents.len());

    // Open each source path once and build the transport extents in
    // descriptor order.
    for extent in parsed_extents {
        let extent_path = descriptor_directory.join(&extent.path);
        let file_index = if let Some(&file_index) = file_indices.get(&extent_path) {
            // Reuse the existing handle when another extent names the same
            // source path.
            file_index
        } else {
            // Assign the next handle index before opening this source for the
            // first time.
            let source_index = files.len();
            let file_index = u32::try_from(source_index)
                .map_err(|_| OpenError::InvalidExtentMap("too many source files".into()))?;
            let file = File::open(&extent_path).map_err(|source| OpenError::OpenExtent {
                path: extent_path.clone(),
                source,
            })?;

            // Error out if the source is not a regular file.
            if !file
                .metadata()
                .map_err(|source| OpenError::ReadExtentMetadata {
                    index: source_index,
                    source,
                })?
                .is_file()
            {
                return Err(OpenError::ExtentNotRegularFile(source_index));
            }

            // Add the file and index to the trackers.
            files.push(file);
            file_indices.insert(extent_path, file_index);
            file_index
        };

        // Preserve descriptor order while referencing the deduplicated source
        // handle table by index.
        extents.push(FlatVmdkExtent {
            file_index,
            access: match extent.access {
                descriptor::ExtentAccess::ReadWrite => VmdkExtentAccess::ReadWrite,
                descriptor::ExtentAccess::ReadOnly => VmdkExtentAccess::ReadOnly,
            },
            sector_count: extent.sector_count,
            source_start_sector: extent.source_start_sector,
        });
    }

    // Reject an invalid layout before transport. The VM worker validates it
    // again because transported resource handles are untrusted.
    FlatVmdkDisk::build_layout(&files, &extents)?;
    Ok(FlatVmdkDiskHandle { files, extents })
}

/// An open VMDK source file and its cached size.
struct Source {
    /// Source file shared by concurrent native and blocking reads.
    file: Arc<File>,
    /// File size in bytes captured when the backend is initialized.
    /// Determines where file data ends and final-sector zero padding begins.
    length: u64,
}

/// Validated extent geometry used to map virtual sectors to a source file.
#[derive(Inspect)]
struct Extent {
    /// Index into [`FlatVmdkDisk::sources`].
    source_index: usize,
    /// First virtual sector provided by this extent.
    virtual_start_sector: u64,
    /// End of this extent in virtual sectors (exclusive).
    virtual_end_sector: u64,
    /// First sector read from the source file.
    source_start_sector: u64,
}

/// Validated data derived from a transported VMDK resource handle.
struct Layout {
    /// Source lengths corresponding to the transported source files.
    source_lengths: Vec<u64>,
    /// Ordered, normalized runtime extents.
    extents: Vec<Extent>,
    /// Total number of virtual sectors across all extents.
    sector_count: u64,
}

/// A contiguous source-file range planned by [`FlatVmdkDisk::plan_read`].
struct ReadSegment {
    /// Index into [`FlatVmdkDisk::sources`] for the read path.
    source_index: usize,
    /// Byte offset in the source file where this read starts.
    source_byte_offset: u64,
    /// Number of output bytes for this segment, including any zero padding.
    byte_count: usize,
}

/// Read-only disk created from a validated flat VMDK layout.
#[derive(Inspect)]
struct FlatVmdkDisk {
    /// Shared table of opened source files and their cached sizes.
    #[inspect(skip)]
    sources: Arc<[Source]>,
    /// Ordered mappings from virtual disk sectors to source-file ranges.
    #[inspect(iter_by_index)]
    extents: Vec<Extent>,
    /// Total virtual disk size, in 512-byte sectors.
    sector_count: u64,
    /// Reusable temporary buffer for assembling blocking reads.
    #[inspect(skip)]
    read_buffer: Mutex<Vec<u8>>,
    /// Shared driver for native reads; absent when io_uring is unavailable.
    #[cfg(target_os = "linux")]
    #[inspect(skip)]
    driver: Option<Box<dyn pal_async::driver::Driver>>,
}

impl FlatVmdkDisk {
    /// Revalidates a transported handle and builds the runtime disk state.
    fn new(
        handle: FlatVmdkDiskHandle,
        #[cfg(target_os = "linux")] driver: Option<Box<dyn pal_async::driver::Driver>>,
    ) -> Result<Self, OpenError> {
        let Layout {
            source_lengths,
            extents,
            sector_count,
        } = Self::build_layout(&handle.files, &handle.extents)?;

        // Pair each opened source file with the length used by runtime reads.
        let sources = handle
            .files
            .into_iter()
            .zip(source_lengths)
            .map(|(file, length)| Source {
                file: Arc::new(file),
                length,
            })
            .collect();

        // Store the driver if io_uring is supported, else None.
        #[cfg(target_os = "linux")]
        let driver = driver.and_then(|driver| {
            if crate::io_uring::is_supported(driver.as_ref()) {
                Some(driver)
            } else {
                tracing::debug!("VMDK native I/O unavailable; using pooled blocking reads");
                None
            }
        });

        Ok(Self {
            sources,
            extents,
            sector_count,
            read_buffer: Mutex::new(Vec::new()),
            #[cfg(target_os = "linux")]
            driver,
        })
    }

    /// Validates opened source files and extent records, then builds the runtime layout.
    fn build_layout(files: &[File], extents: &[FlatVmdkExtent]) -> Result<Layout, OpenError> {
        // A disk must expose at least one extent.
        if extents.is_empty() {
            return Err(OpenError::InvalidExtentMap(
                "the extent map is empty".into(),
            ));
        }

        // Validate every transported source and cache its current length.
        let mut source_lengths = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let metadata = file
                .metadata()
                .map_err(|source| OpenError::ReadExtentMetadata { index, source })?;

            if !metadata.is_file() {
                return Err(OpenError::ExtentNotRegularFile(index));
            }

            source_lengths.push(metadata.len());
        }

        // Validate each transported extent and normalize its runtime geometry.
        let mut current_virtual_sector_count = 0u64;
        let mut layout_extents = Vec::with_capacity(extents.len());
        for (index, extent) in extents.iter().enumerate() {
            // Convert the transported file index to a native vector index.
            // The source-table bounds check follows below.
            let source_index = usize::try_from(extent.file_index).map_err(|_| {
                OpenError::InvalidExtentMap(format!("extent {index} has an invalid source"))
            })?;
            let source_length = source_lengths.get(source_index).copied().ok_or_else(|| {
                OpenError::InvalidExtentMap(format!(
                    "extent {index} references missing source file {source_index}"
                ))
            })?;

            if extent.sector_count == 0 {
                return Err(OpenError::InvalidExtentMap(format!(
                    "extent {index} has zero sectors"
                )));
            }

            // Ensure the requested source range fits, allowing one partial
            // final source sector.
            let source_end = extent
                .source_start_sector
                .checked_add(extent.sector_count)
                .ok_or_else(|| {
                    OpenError::InvalidExtentMap(format!("extent {index} source range overflows"))
                })?;
            let source_sector_count = source_length.div_ceil(SECTOR_SIZE);
            if source_end > source_sector_count {
                return Err(OpenError::InvalidExtentMap(format!(
                    "extent {index} ends at source sector {source_end}, but source file {source_index} has only {source_sector_count} sectors"
                )));
            }

            // Append this extent at the next virtual sector without exceeding
            // the host file-offset limit.
            let next_virtual_sector_count = current_virtual_sector_count
                .checked_add(extent.sector_count)
                .filter(|count| *count <= MAX_SECTOR_COUNT)
                .ok_or_else(|| {
                    OpenError::InvalidExtentMap("virtual disk size is too large".into())
                })?;

            layout_extents.push(Extent {
                source_index,
                virtual_start_sector: current_virtual_sector_count,
                virtual_end_sector: next_virtual_sector_count,
                source_start_sector: extent.source_start_sector,
            });

            // Advance the virtual sector counter to the next available sector.
            current_virtual_sector_count = next_virtual_sector_count;
        }

        Ok(Layout {
            source_lengths,
            extents: layout_extents,
            sector_count: current_virtual_sector_count,
        })
    }

    /// Reads a request using native I/O when available or the blocking fallback.
    async fn read(&self, buffers: &RequestBuffers<'_>, sector: u64) -> Result<(), DiskError> {
        let request_byte_count = buffers.len();

        // Validate the whole-sector request and disk bounds before choosing a read path.
        if !request_byte_count.is_multiple_of(SECTOR_SIZE as usize) {
            return Err(DiskError::InvalidInput);
        }
        let request_sector_count = (request_byte_count / SECTOR_SIZE as usize) as u64;
        let request_end_sector = sector
            .checked_add(request_sector_count)
            .filter(|end| *end <= self.sector_count)
            .ok_or(DiskError::IllegalBlock)?;

        // Find the last extent starting at or before the requested sector.
        // The first extent starts at sector 0, so subtracting 1 is safe.
        let extent_index = self
            .extents
            .partition_point(|extent| extent.virtual_start_sector <= sector)
            - 1;

        // Check if native IO is supported.
        #[cfg(target_os = "linux")]
        let driver = self
            .driver
            .as_deref()
            .filter(|_| !buffers.is_empty() && buffers.guest_memory().supports_locking());

        // Fast path: if the read fits within one extent, use native I/O without building a segment list.
        #[cfg(target_os = "linux")]
        if let Some(driver) = driver {
            let extent = &self.extents[extent_index];
            if request_end_sector <= extent.virtual_end_sector {
                // Translate the virtual sector to a byte offset in the source file.
                // Example: the extent starts at virtual sector 10 and source sector 2.
                // Requesting virtual sector 13 is 3 sectors into the extent.
                // The source byte offset is (2 + 3) * 512 = 2560.
                let source_byte_offset = (extent.source_start_sector
                    + (sector - extent.virtual_start_sector))
                    * SECTOR_SIZE;

                // Read into guest memory, padding a partial final sector if needed.
                return Self::read_native_range(
                    driver,
                    &self.sources[extent.source_index],
                    buffers.clone(),
                    source_byte_offset,
                )
                .await;
            }
        }

        // The fast path was not used; split the request into reads from the backing files.
        let segments = self.plan_read(sector, request_end_sector, extent_index);

        // Read the planned source ranges directly into guest memory.
        #[cfg(target_os = "linux")]
        if let Some(driver) = driver {
            let mut completed_request_byte_count = 0;
            // Set by `fold` below when a read fails; checked before each new read.
            let failed = AtomicBool::new(false);

            // Lazily build one read per segment. Nothing runs here: `buffered`
            // below pulls each read only when it has room for another.
            let reads = segments
                .into_iter()
                // Checked at pull time, so reads after a failure are never built.
                .take_while(|_| !failed.load(Ordering::Relaxed))
                .map(|segment| {
                    // Select the guest buffer range that receives this segment.
                    let segment_output =
                        buffers.subrange(completed_request_byte_count, segment.byte_count);

                    // Advance past this segment for the next one.
                    completed_request_byte_count += segment.byte_count;

                    // Create the read future; I/O starts when it is first polled.
                    Self::read_native_range(
                        driver,
                        &self.sources[segment.source_index],
                        segment_output,
                        segment.source_byte_offset,
                    )
                });

            // Run up to MAX_READ_SUB_WORKERS reads at once. After a failure, no
            // new reads start, but in-flight ones finish (dropping them aborts).
            return futures::stream::iter(reads)
                .buffered(MAX_READ_SUB_WORKERS)
                .fold(Ok(()), |result, read_result| {
                    if read_result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    // Keep the earliest error.
                    async move { result.and(read_result) }
                })
                .await;
        }

        // At this point, native IO was not used, so fall back to blocking reads.
        // Try to reuse the cached buffer; larger reads get a separate allocation.
        let mut request_output = if request_byte_count <= MAX_CACHED_READ_BUFFER_SIZE {
            std::mem::take(&mut *self.read_buffer.lock())
        } else {
            Vec::new()
        };
        request_output.resize(request_byte_count, 0);

        // Share the source files with the worker thread.
        let sources = self.sources.clone();

        // Read on a worker thread so file I/O does not block the async executor.
        let (request_output, read_result) = blocking::unblock(move || {
            let result = Self::read_segments_blocking(&sources, &mut request_output, segments);

            // Return the buffer even on failure so it can be cached for reuse.
            (request_output, result)
        })
        .await;

        // Copy data to the guest only after a successful read. Keep any error
        // for the final return so the temporary buffer can still be reused.
        let result = read_result.map_err(DiskError::Io).and_then(|()| {
            buffers
                .writer()
                .write(&request_output)
                .map_err(DiskError::from)
        });

        // Cache this buffer if it fits the size limit and has more capacity than the cached one.
        if request_output.capacity() <= MAX_CACHED_READ_BUFFER_SIZE {
            let mut read_buffer = self.read_buffer.lock();
            if request_output.capacity() > read_buffer.capacity() {
                *read_buffer = request_output;
            }
        }

        result
    }

    /// Plans source ranges for a validated request, starting at `extent_index`.
    fn plan_read(
        &self,
        request_start_sector: u64,
        request_end_sector: u64,
        mut extent_index: usize,
    ) -> Vec<ReadSegment> {
        let mut current_virtual_sector = request_start_sector;
        let mut segments = Vec::new();

        while current_virtual_sector < request_end_sector {
            // The validated layout covers every sector up to sector_count.
            let extent = &self.extents[extent_index];

            // Limit this segment to the part covered by the current extent.
            let segment_sector_count = (request_end_sector - current_virtual_sector)
                .min(extent.virtual_end_sector - current_virtual_sector);

            // Translate the virtual sector into a sector in the source file.
            let source_sector =
                extent.source_start_sector + (current_virtual_sector - extent.virtual_start_sector);

            // Record the source range in virtual-disk order.
            segments.push(ReadSegment {
                source_index: extent.source_index,
                source_byte_offset: source_sector * SECTOR_SIZE,
                byte_count: segment_sector_count as usize * SECTOR_SIZE as usize,
            });

            // Advance past the segment just planned.
            current_virtual_sector += segment_sector_count;
            extent_index += 1;
        }

        segments
    }

    /// Reads a source range and pads only bytes beyond its cached length.
    #[cfg(target_os = "linux")]
    async fn read_native_range(
        driver: &dyn pal_async::driver::Driver,
        source: &Source,
        segment_output: RequestBuffers<'_>,
        source_byte_offset: u64,
    ) -> Result<(), DiskError> {
        // The file's last sector may contain fewer than 512 bytes.
        // Read the file bytes in this range; the rest will be filled with zeros.
        // Example: with a cached file size of 513 bytes, a 512-byte read at offset
        // 512 reads 1 file byte and fills the remaining 511 bytes with zeros.
        let readable_byte_count = source
            .length
            .saturating_sub(source_byte_offset)
            .min(segment_output.len() as u64) as usize;

        // Missing bytes inside this range are a truncation error, not padding.
        crate::io_uring::read_exact_at(
            driver,
            source.file.clone(),
            &segment_output.subrange(0, readable_byte_count),
            source_byte_offset,
        )
        .await?;

        // Zero only the final sector's padding, preserving the file bytes just read.
        if readable_byte_count < segment_output.len() {
            segment_output
                .subrange(
                    readable_byte_count,
                    segment_output.len() - readable_byte_count,
                )
                .writer()
                .zero(segment_output.len() - readable_byte_count)?;
        }

        Ok(())
    }

    /// Reads planned ranges with blocking positional I/O and zero padding.
    fn read_segments_blocking(
        sources: &[Source],
        mut request_output: &mut [u8],
        segments: Vec<ReadSegment>,
    ) -> io::Result<()> {
        for segment in segments {
            let source = &sources[segment.source_index];

            // Take this segment's bytes, leaving the rest for later segments.
            let segment_output = request_output
                .split_off_mut(..segment.byte_count)
                .ok_or_else(|| io::Error::other("VMDK read segment exceeds request buffer"))?;

            // Read up to the cached end of the file; the rest of the segment is padding.
            let bytes_left_in_file = source.length.saturating_sub(segment.source_byte_offset);
            let readable_byte_count = bytes_left_in_file.min(segment_output.len() as u64) as usize;
            let (mut destination, padding) = segment_output.split_at_mut(readable_byte_count);

            // Zero the padding, including any old data from a reused buffer.
            padding.fill(0);

            // Read at explicit file offsets until this segment is filled.
            let mut source_byte_offset = segment.source_byte_offset;
            while !destination.is_empty() {
                #[cfg(unix)]
                let result = source.file.read_at(destination, source_byte_offset);
                #[cfg(windows)]
                let result = source.file.seek_read(destination, source_byte_offset);

                match result {
                    Ok(0) => {
                        // Bytes expected from the cached length are now missing.
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "VMDK source file was truncated",
                        ));
                    }
                    Ok(byte_count) => {
                        // Continue after the bytes returned by a partial read.
                        destination = &mut destination[byte_count..];
                        source_byte_offset += byte_count as u64;
                    }
                    // Retry interruptions without advancing either position.
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }

        // Reject incomplete plans rather than returning stale buffer contents.
        if !request_output.is_empty() {
            return Err(io::Error::other(
                "VMDK read segments do not fill the request buffer",
            ));
        }

        Ok(())
    }
}

impl DiskIo for FlatVmdkDisk {
    fn disk_type(&self) -> &str {
        "vmdk_flat"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        SECTOR_SIZE as u32
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        DEFAULT_PHYSICAL_SECTOR_SIZE
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.read(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::ReadOnly)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }
}
