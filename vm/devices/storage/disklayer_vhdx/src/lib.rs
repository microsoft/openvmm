// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX disk layer for OpenVMM.
//!
//! Provides a cross-platform, pure-Rust VHDX backend for the layered disk
//! stack. Uses the `vhdx` crate for format parsing and the `disk_layered`
//! crate's `LayerIo` trait for integration.
//!
//! # Modules
//!
//! - [`io`] — owned-buffer metadata I/O adapter

#![forbid(unsafe_code)]

pub mod chain;
pub mod io;
pub mod resolver;

use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_file::host_file::HostFile;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use guestmem::MemoryWrite;
use inspect::Inspect;
use io::MetadataFile;
use scsi_buffers::RequestBuffers;
use vhdx::ReadRange;
use vhdx::VhdxFile;
use vhdx::WriteRange;

/// A VHDX disk layer implementing [`LayerIo`].
///
/// Bridges the `vhdx` crate's range-based I/O model ([`ReadRange`]/[`WriteRange`])
/// to the `disk_layered` crate's buffer-based I/O model ([`RequestBuffers`] +
/// [`SectorMarker`]).
#[derive(Inspect)]
pub struct VhdxLayer {
    #[inspect(skip)]
    vhdx: VhdxFile<MetadataFile>,
    #[inspect(skip)]
    payload_file: HostFile,
    sector_size: u32,
    physical_sector_size: u32,
    sector_count: u64,
    block_size: u32,
    has_parent: bool,
    read_only: bool,
}

impl VhdxLayer {
    /// Creates a layer from an open VHDX and its shared payload scheduler.
    pub fn new(vhdx: VhdxFile<MetadataFile>, payload_file: HostFile, read_only: bool) -> Self {
        let sector_size = vhdx.logical_sector_size();
        let physical_sector_size = vhdx.physical_sector_size();
        let sector_count = vhdx.disk_size() / sector_size as u64;
        let block_size = vhdx.block_size();
        let has_parent = vhdx.has_parent();
        Self {
            vhdx,
            payload_file,
            sector_size,
            physical_sector_size,
            sector_count,
            block_size,
            has_parent,
            read_only,
        }
    }
}

/// Convert a [`vhdx::VhdxIoError`] to a [`DiskError`].
fn vhdx_to_disk_error(e: vhdx::VhdxIoError) -> DiskError {
    match e.kind() {
        vhdx::VhdxIoErrorKind::ReadOnly => DiskError::ReadOnly,
        vhdx::VhdxIoErrorKind::InvalidInput => DiskError::InvalidInput,
        vhdx::VhdxIoErrorKind::InvalidSector => DiskError::IllegalBlock,
        _ => DiskError::Io(std::io::Error::other(e)),
    }
}

impl LayerIo for VhdxLayer {
    fn layer_type(&self) -> &str {
        "vhdx"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        Some(self.vhdx.page_83_data().into())
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    fn is_logically_read_only(&self) -> bool {
        self.read_only
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.block_size / self.sector_size
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        if self.has_parent {
            UnmapBehavior::Unspecified
        } else {
            UnmapBehavior::Zeroes
        }
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.vhdx.flush().await.map_err(vhdx_to_disk_error)
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        let offset = sector * self.sector_size as u64;
        let len = buffers.len() as u32;

        // Resolve the read into file-level ranges.
        let mut ranges = Vec::new();
        let guard = self
            .vhdx
            .resolve_read(offset, len, &mut ranges)
            .await
            .map_err(vhdx_to_disk_error)?;
        let payload_request = self.payload_file.request(buffers.len());

        // Process each range.
        for range in &ranges {
            match *range {
                ReadRange::Data {
                    guest_offset,
                    length,
                    file_offset,
                } => {
                    let buf_offset = (guest_offset - offset) as usize;
                    payload_request
                        .read_at(&buffers.subrange(buf_offset, length as usize), file_offset)
                        .await?;

                    // Mark these sectors as present.
                    let start_sector = guest_offset / self.sector_size as u64;
                    let sector_count = length as u64 / self.sector_size as u64;
                    marker.set_range(start_sector..start_sector + sector_count);
                }
                ReadRange::Zero {
                    guest_offset,
                    length,
                } => {
                    let buf_offset = (guest_offset - offset) as usize;

                    // Zero this portion of the request buffers.
                    buffers
                        .subrange(buf_offset, length as usize)
                        .writer()
                        .zero(length as usize)?;

                    // Mark these sectors as present (they are definitively zero).
                    let start_sector = guest_offset / self.sector_size as u64;
                    let sector_count = length as u64 / self.sector_size as u64;
                    marker.set_range(start_sector..start_sector + sector_count);
                }
                ReadRange::Unmapped { .. } => {
                    // Do NOT mark these sectors. LayeredDisk will read from
                    // the next layer down in the stack.
                }
            }
        }

        // Drop the guard (decrements per-block refcounts).
        drop(guard);

        Ok(())
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let offset = sector * self.sector_size as u64;
        let len = buffers.len() as u32;

        // Resolve the write into file-level ranges.
        let mut ranges = Vec::new();
        let guard = self
            .vhdx
            .resolve_write(offset, len, &mut ranges)
            .await
            .map_err(vhdx_to_disk_error)?;
        let payload_request = self.payload_file.request(buffers.len());

        // Process each range.
        for range in &ranges {
            match *range {
                WriteRange::Data {
                    guest_offset,
                    length,
                    file_offset,
                } => {
                    let buf_offset = (guest_offset - offset) as usize;
                    payload_request
                        .write_at(
                            &buffers.subrange(buf_offset, length as usize),
                            file_offset,
                            false,
                        )
                        .await?;
                }
                WriteRange::Zero {
                    file_offset,
                    length,
                } => {
                    self.payload_file
                        .zero_at(file_offset, length as u64)
                        .await?;
                }
            }
        }

        // Complete the write (commits TFP blocks, updates sector bitmaps).
        guard.complete().await.map_err(vhdx_to_disk_error)?;

        // If FUA, flush to stable storage.
        if fua {
            self.vhdx.flush().await.map_err(vhdx_to_disk_error)?;
        }

        Ok(())
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        _block_level_only: bool,
        _next_is_zero: bool,
    ) -> Result<(), DiskError> {
        let offset = sector * self.sector_size as u64;
        let length = count * self.sector_size as u64;

        // Use TrimMode::Zero for base disks (unmapped reads will return zero),
        // TrimMode::MakeTransparent for diff disks (reads fall through to parent).
        let mode = if self.has_parent {
            vhdx::TrimMode::MakeTransparent
        } else {
            vhdx::TrimMode::Zero
        };

        self.vhdx
            .trim(vhdx::TrimRequest::new(mode, offset, length))
            .await
            .map_err(vhdx_to_disk_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_backend::Disk;
    use disk_backend::DiskIo;
    use disk_layered::DiskLayer;
    use disk_layered::LayerConfiguration;
    use disk_layered::LayeredDisk;
    use guestmem::GuestMemory;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use vhdx::VhdxFile;

    /// Create a VHDX file at the given path and return a `VhdxLayer`.
    async fn create_and_open_layer(path: &std::path::Path, driver: &DefaultDriver) -> VhdxLayer {
        // Create a 1 MiB VHDX.
        let bf = MetadataFile::open(path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&bf, &mut params).await.unwrap();

        // Re-open and wrap as VhdxLayer.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let payload_file = HostFile::new(file, driver.clone()).unwrap();
        let metadata_file = MetadataFile::from_owned(payload_file.owned_file());
        let vhdx = VhdxFile::open(metadata_file)
            .writable(driver)
            .await
            .unwrap();
        VhdxLayer::new(vhdx, payload_file, false)
    }

    /// Wrap a VhdxLayer in a single-layer LayeredDisk.
    async fn wrap_in_layered_disk(layer: VhdxLayer) -> LayeredDisk {
        LayeredDisk::new(
            false,
            vec![LayerConfiguration {
                layer: DiskLayer::new(layer),
                write_through: false,
                read_cache: false,
            }],
        )
        .await
        .unwrap()
    }

    #[async_test]
    async fn sector_range_conformance(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");
        let layer = create_and_open_layer(&path, &driver).await;
        let disk = Disk::new(wrap_in_layered_disk(layer).await).unwrap();
        storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
    }

    /// `LayeredDisk` does not reach `write_no_overwrite` or both values of
    /// `unmap`'s `next_is_zero`, so the layer is also tested directly.
    #[async_test]
    async fn layer_sector_range_conformance(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");
        let layer = create_and_open_layer(&path, &driver).await;
        storage_tests::sector_range::test_layer_sector_range(&layer).await;
    }

    #[async_test]
    async fn read_empty_disk_via_layer(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let layer = create_and_open_layer(&path, &driver).await;

        // Verify metadata.
        assert_eq!(layer.sector_size(), 512);
        assert_eq!(layer.sector_count(), 1024 * 1024 / 512);
        assert_eq!(layer.layer_type(), "vhdx");
        assert!(!layer.is_logically_read_only());

        let disk = wrap_in_layered_disk(layer).await;

        // Read sector 0 — empty disk should return all zeros.
        let mem = GuestMemory::allocate(512);
        let owned = OwnedRequestBuffers::linear(0, 512, true);
        disk.read_vectored(&owned.buffer(&mem), 0).await.unwrap();

        let mut buf = vec![0u8; 512];
        mem.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0u8; 512]);
    }

    #[async_test]
    async fn write_and_read_back(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let layer = create_and_open_layer(&path, &driver).await;
        let disk = wrap_in_layered_disk(layer).await;

        // Write a known pattern to sector 0.
        let mem = GuestMemory::allocate(512);
        let pattern: Vec<u8> = (0..512u16).map(|i| (i % 251) as u8).collect();
        mem.write_at(0, &pattern).unwrap();
        let owned = OwnedRequestBuffers::linear(0, 512, false);
        disk.write_vectored(&owned.buffer(&mem), 0, false)
            .await
            .unwrap();

        // Read back sector 0, verify data matches.
        let owned = OwnedRequestBuffers::linear(0, 512, true);
        disk.read_vectored(&owned.buffer(&mem), 0).await.unwrap();
        let mut buf = vec![0u8; 512];
        mem.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, pattern);

        // Read sector 1 — should be zero.
        disk.read_vectored(&owned.buffer(&mem), 1).await.unwrap();
        mem.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0u8; 512]);
    }

    #[async_test]
    async fn sync_cache_works(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let layer = create_and_open_layer(&path, &driver).await;
        let disk = wrap_in_layered_disk(layer).await;

        disk.sync_cache().await.unwrap();
    }

    #[async_test]
    async fn write_close_reopen_read(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");
        let len = 128 * 1024;

        // Create and write data
        {
            let layer = create_and_open_layer(&path, &driver).await;
            let disk = wrap_in_layered_disk(layer).await;

            let mem = GuestMemory::allocate(len);
            let pattern: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            mem.write_at(0, &pattern).unwrap();
            let owned = OwnedRequestBuffers::linear(0, len, false);
            disk.write_vectored(&owned.buffer(&mem), 0, true)
                .await
                .unwrap();

            // Flush to ensure data is on disk
            disk.sync_cache().await.unwrap();
        }

        // Re-open and read back
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let payload_file = HostFile::new(file, driver.clone()).unwrap();
            let metadata_file = MetadataFile::from_owned(payload_file.owned_file());
            let vhdx = VhdxFile::open(metadata_file)
                .allow_replay(true)
                .read_only()
                .await
                .unwrap();
            let layer = VhdxLayer::new(vhdx, payload_file, true);
            let disk = LayeredDisk::new(
                true,
                vec![LayerConfiguration {
                    layer: DiskLayer::new(layer),
                    write_through: false,
                    read_cache: false,
                }],
            )
            .await
            .unwrap();

            let mem = GuestMemory::allocate(len);
            let owned = OwnedRequestBuffers::linear(0, len, true);
            disk.read_vectored(&owned.buffer(&mem), 0).await.unwrap();

            let mut buf = vec![0u8; len];
            mem.read_at(0, &mut buf).unwrap();
            let expected: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert_eq!(buf, expected);
        }
    }

    #[async_test]
    async fn multi_sector_write_and_read(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let layer = create_and_open_layer(&path, &driver).await;
        let disk = wrap_in_layered_disk(layer).await;

        // Write 4 KiB (8 sectors) starting at sector 0
        let len = 4096usize;
        let mem = GuestMemory::allocate(len);
        let pattern: Vec<u8> = (0..len).map(|i| (i % 137) as u8).collect();
        mem.write_at(0, &pattern).unwrap();

        let owned = OwnedRequestBuffers::linear(0, len, false);
        disk.write_vectored(&owned.buffer(&mem), 0, false)
            .await
            .unwrap();

        // Read back and verify
        let owned = OwnedRequestBuffers::linear(0, len, true);
        disk.read_vectored(&owned.buffer(&mem), 0).await.unwrap();

        let mut buf = vec![0u8; len];
        mem.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, pattern);
    }
}
