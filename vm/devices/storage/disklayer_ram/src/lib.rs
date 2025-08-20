// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RAM-backed disk layer implementation.

#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::DiskLayer;
use disk_layered::LayerAttach;
use disk_layered::LayerConfiguration;
use disk_layered::LayerIo;
use disk_layered::LayeredDisk;
use disk_layered::SectorMarker;
use disk_layered::WriteNoOverwrite;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::RwLock;
use scsi_buffers::RequestBuffers;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::fmt::Debug;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;

/// A disk layer backed by RAM, which lazily infers its topology from the layer
/// it is being stacked on-top of
#[non_exhaustive]
pub struct LazyRamDiskLayer {}

impl LazyRamDiskLayer {
    /// Create a new lazy RAM-backed disk layer
    pub fn new() -> Self {
        Self {}
    }
}

/// A disk layer backed entirely by RAM.
#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub struct RamDiskLayer {
    #[inspect(flatten)]
    state: RwLock<RamState>,
    #[inspect(skip)]
    sector_count: AtomicU64,
    #[inspect(skip)]
    sector_size: u32,
    #[inspect(skip)]
    resize_event: event_listener::Event,
}

#[derive(Inspect)]
struct RamState {
    #[inspect(skip)]
    data: BTreeMap<u64, Sector>,
    #[inspect(skip)] // handled in inspect_extra()
    sector_count: u64,
    zero_after: u64,
}

impl RamDiskLayer {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field_with("committed_size", || {
            self.state.read().data.len() * self.sector_size as usize
        })
        .field_mut_with("sector_count", |new_count| {
            if let Some(new_count) = new_count {
                self.resize(new_count.parse().context("invalid sector count")?)?;
            }
            anyhow::Ok(self.sector_count())
        });
    }
}

impl Debug for RamDiskLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RamDiskLayer")
            .field("sector_count", &self.sector_count)
            .field("sector_size", &self.sector_size)
            .finish()
    }
}

/// An error creating a RAM disk.
#[derive(Error, Debug)]
pub enum Error {
    /// The disk size is not a multiple of the sector size.
    #[error("disk size {disk_size:#x} is not a multiple of the sector size {sector_size}")]
    NotSectorMultiple {
        /// The disk size.
        disk_size: u64,
        /// The sector size.
        sector_size: u32,
    },
    /// The disk has no sectors.
    #[error("disk has no sectors")]
    EmptyDisk,
}

/// Dynamic sector data that can hold different sector sizes
struct Sector(Vec<u8>);

/// Default sector size (512 bytes) for backward compatibility
const DEFAULT_SECTOR_SIZE: u32 = 512;

impl RamDiskLayer {
    /// Makes a new RAM disk layer of `size` bytes with default 512-byte sectors.
    pub fn new(size: u64) -> Result<Self, Error> {
        Self::new_with_sector_size(size, DEFAULT_SECTOR_SIZE)
    }

    /// Makes a new RAM disk layer of `size` bytes with the specified sector size.
    ///
    /// # Arguments
    /// * `size` - Total size of the disk in bytes
    /// * `sector_size` - Size of each sector in bytes (typically 512 or 4096)
    pub fn new_with_sector_size(size: u64, sector_size: u32) -> Result<Self, Error> {
        let sector_count = {
            if size == 0 {
                return Err(Error::EmptyDisk);
            }
            if sector_size == 0 {
                return Err(Error::NotSectorMultiple {
                    disk_size: size,
                    sector_size,
                });
            }
            if size % sector_size as u64 != 0 {
                return Err(Error::NotSectorMultiple {
                    disk_size: size,
                    sector_size,
                });
            }
            size / sector_size as u64
        };
        Ok(Self {
            state: RwLock::new(RamState {
                data: BTreeMap::new(),
                sector_count,
                zero_after: sector_count,
            }),
            sector_count: sector_count.into(),
            sector_size,
            resize_event: Default::default(),
        })
    }

    fn resize(&self, new_sector_count: u64) -> anyhow::Result<()> {
        if new_sector_count == 0 {
            anyhow::bail!("invalid sector count");
        }
        // Remove any truncated data and update the sector count under the lock.
        let _removed = {
            let mut state = self.state.write();
            // Remember that any non-present sectors after this point need to be zeroed.
            state.zero_after = new_sector_count.min(state.zero_after);
            state.sector_count = new_sector_count;
            // Cache the sector count in an atomic for the fast path.
            //
            // FUTURE: remove uses of .sector_count() in the IO path,
            // eliminating the need for this.
            self.sector_count.store(new_sector_count, Ordering::Relaxed);
            state.data.split_off(&new_sector_count)
        };
        self.resize_event.notify(usize::MAX);
        Ok(())
    }

    fn write_maybe_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        overwrite: bool,
    ) -> Result<(), DiskError> {
        let count = buffers.len() / self.sector_size as usize;
        tracing::trace!(sector, count, "write");
        let mut state = self.state.write();
        if sector + count as u64 > state.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        for i in 0..count {
            let cur = i + sector as usize;
            let buf = buffers.subrange(i * self.sector_size as usize, self.sector_size as usize);
            let mut reader = buf.reader();
            match state.data.entry(cur as u64) {
                Entry::Vacant(entry) => {
                    let mut sector_data = vec![0u8; self.sector_size as usize];
                    reader.read(&mut sector_data)?;
                    entry.insert(Sector(sector_data));
                }
                Entry::Occupied(mut entry) => {
                    if overwrite {
                        reader.read(&mut entry.get_mut().0)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl LayerAttach for LazyRamDiskLayer {
    type Error = Error;
    type Layer = RamDiskLayer;

    async fn attach(
        self,
        lower_layer_metadata: Option<disk_layered::DiskLayerMetadata>,
    ) -> Result<Self::Layer, Self::Error> {
        RamDiskLayer::new(
            lower_layer_metadata
                .map(|x| x.sector_count * x.sector_size as u64)
                .ok_or(Error::EmptyDisk)?,
        )
    }
}

impl LayerIo for RamDiskLayer {
    fn layer_type(&self) -> &str {
        "ram"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count.load(Ordering::Relaxed)
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_logically_read_only(&self) -> bool {
        false
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        let count = (buffers.len() / self.sector_size as usize) as u64;
        let end = sector + count;
        tracing::trace!(sector, count, "read");
        let state = self.state.read();
        if end > state.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        let mut range = state.data.range(sector..end);
        let mut last = sector;
        while last < end {
            let r = range.next();
            let next = r.map(|(&s, _)| s).unwrap_or(end);
            if next > last && next > state.zero_after {
                // Some non-present sectors need to be zeroed, since they are
                // after the zero-after point (due to a resize).
                let zero_start = last.max(state.zero_after);
                let zero_count = next - zero_start;
                let offset = (zero_start - sector) as usize * self.sector_size as usize;
                let len = zero_count as usize * self.sector_size as usize;
                buffers.subrange(offset, len).writer().zero(len)?;
                marker.set_range(zero_start..next);
            }
            if let Some((&s, buf)) = r {
                let offset = (s - sector) as usize * self.sector_size as usize;
                buffers
                    .subrange(offset, self.sector_size as usize)
                    .writer()
                    .write(&buf.0)?;

                marker.set(s);
            }
            last = next;
        }
        Ok(())
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, true)
    }

    fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
        Some(self)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        tracing::trace!("sync_cache");
        Ok(())
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        loop {
            let listen = self.resize_event.listen();
            let current = self.sector_count();
            if current != sector_count {
                break current;
            }
            listen.await;
        }
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
        next_is_zero: bool,
    ) -> Result<(), DiskError> {
        tracing::trace!(sector_offset, sector_count, "unmap");
        let mut state = self.state.write();
        if sector_offset + sector_count > state.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        if !next_is_zero {
            // This would create a hole of zeroes, which we cannot represent in
            // the tree. Ignore the unmap.
            if sector_offset + sector_count < state.zero_after {
                return Ok(());
            }
            // The unmap is within or will extend the not-present-is-zero
            // region, so allow it.
            state.zero_after = state.zero_after.min(sector_offset);
        }
        // Sadly, there appears to be no way to remove a range of entries
        // from a btree map.
        let mut next_sector = sector_offset;
        let end = sector_offset + sector_count;
        while next_sector < end {
            let Some((&sector, _)) = state.data.range_mut(next_sector..).next() else {
                break;
            };
            if sector >= end {
                break;
            }
            state.data.remove(&sector);
            next_sector = sector + 1;
        }
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        // This layer zeroes if the lower layer is zero, but otherwise does
        // nothing, so we must report unspecified.
        UnmapBehavior::Unspecified
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }
}

impl WriteNoOverwrite for RamDiskLayer {
    async fn write_no_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, false)
    }
}

/// Create a RAM disk of `size` bytes with default 512-byte sectors.
///
/// This is a convenience function for creating a layered disk with a single RAM
/// layer. It is useful since non-layered RAM disks are used all over the place,
/// especially in tests.
pub fn ram_disk(size: u64, read_only: bool) -> anyhow::Result<Disk> {
    ram_disk_with_sector_size(size, read_only, DEFAULT_SECTOR_SIZE)
}

/// Create a RAM disk of `size` bytes with the specified sector size.
///
/// This is a convenience function for creating a layered disk with a single RAM
/// layer with configurable sector size. Useful for testing different sector sizes.
///
/// # Arguments
/// * `size` - Total size of the disk in bytes
/// * `read_only` - Whether the disk should be read-only
/// * `sector_size` - Size of each sector in bytes (typically 512 or 4096)
pub fn ram_disk_with_sector_size(
    size: u64,
    read_only: bool,
    sector_size: u32,
) -> anyhow::Result<Disk> {
    use futures::future::FutureExt;

    let disk = Disk::new(
        LayeredDisk::new(
            read_only,
            vec![LayerConfiguration {
                layer: DiskLayer::new(RamDiskLayer::new_with_sector_size(size, sector_size)?),
                write_through: false,
                read_cache: false,
            }],
        )
        .now_or_never()
        .expect("RamDiskLayer won't block")?,
    )?;

    Ok(disk)
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_SECTOR_SIZE;
    use super::RamDiskLayer;
    use disk_backend::Disk;
    use disk_backend::DiskIo;
    use disk_layered::DiskLayer;
    use disk_layered::LayerConfiguration;
    use disk_layered::LayerIo;
    use disk_layered::LayeredDisk;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use test_with_tracing::test;
    use zerocopy::IntoBytes;

    const SECTOR_U64: u64 = DEFAULT_SECTOR_SIZE as u64;
    const SECTOR_USIZE: usize = DEFAULT_SECTOR_SIZE as usize;

    fn check(mem: &GuestMemory, sector: u64, start: usize, count: usize, high: u8) {
        let mut buf = vec![0u32; count * SECTOR_USIZE / 4];
        mem.read_at(start as u64 * SECTOR_U64, buf.as_mut_bytes())
            .unwrap();
        for (i, &b) in buf.iter().enumerate() {
            let offset = sector * SECTOR_U64 + i as u64 * 4;
            let expected = (offset as u32 / 4) | ((high as u32) << 24);
            assert!(
                b == expected,
                "at sector {}, word {}, got {:#x}, expected {:#x}",
                offset / SECTOR_U64,
                (offset % SECTOR_U64) / 4,
                b,
                expected
            );
        }
    }

    async fn read(mem: &GuestMemory, disk: &mut impl DiskIo, sector: u64, count: usize) {
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, count * SECTOR_USIZE, true).buffer(mem),
            sector,
        )
        .await
        .unwrap();
    }

    async fn write_layer(
        mem: &GuestMemory,
        disk: &mut impl LayerIo,
        sector: u64,
        count: usize,
        high: u8,
    ) {
        let buf: Vec<_> = (sector * SECTOR_U64 / 4..(sector + count as u64) * SECTOR_U64 / 4)
            .map(|x| x as u32 | ((high as u32) << 24))
            .collect();
        let len = SECTOR_USIZE * count;
        mem.write_at(0, &buf.as_bytes()[..len]).unwrap();

        disk.write(
            &OwnedRequestBuffers::linear(0, len, false).buffer(mem),
            sector,
            false,
        )
        .await
        .unwrap();
    }

    async fn write(mem: &GuestMemory, disk: &mut impl DiskIo, sector: u64, count: usize, high: u8) {
        let buf: Vec<_> = (sector * SECTOR_U64 / 4..(sector + count as u64) * SECTOR_U64 / 4)
            .map(|x| x as u32 | ((high as u32) << 24))
            .collect();
        let len = SECTOR_USIZE * count;
        mem.write_at(0, &buf.as_bytes()[..len]).unwrap();

        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, len, false).buffer(mem),
            sector,
            false,
        )
        .await
        .unwrap();
    }

    async fn prep_disk(size: usize) -> (GuestMemory, LayeredDisk) {
        let guest_mem = GuestMemory::allocate(size);
        let mut lower = RamDiskLayer::new(size as u64).unwrap();
        write_layer(&guest_mem, &mut lower, 0, size / SECTOR_USIZE, 0).await;
        let upper = RamDiskLayer::new(size as u64).unwrap();
        let upper = LayeredDisk::new(
            false,
            Vec::from_iter([upper, lower].map(|layer| LayerConfiguration {
                layer: DiskLayer::new(layer),
                write_through: false,
                read_cache: false,
            })),
        )
        .await
        .unwrap();
        (guest_mem, upper)
    }

    #[async_test]
    async fn diff() {
        const SIZE: usize = 1024 * 1024;

        let (guest_mem, mut upper) = prep_disk(SIZE).await;
        read(&guest_mem, &mut upper, 10, 2).await;
        check(&guest_mem, 10, 0, 2, 0);
        write(&guest_mem, &mut upper, 10, 2, 1).await;
        write(&guest_mem, &mut upper, 11, 1, 2).await;
        read(&guest_mem, &mut upper, 9, 5).await;
        check(&guest_mem, 9, 0, 1, 0);
        check(&guest_mem, 10, 1, 1, 1);
        check(&guest_mem, 11, 2, 1, 2);
        check(&guest_mem, 12, 3, 1, 0);
    }

    async fn resize(disk: &LayeredDisk, new_size: u64) {
        let inspect::ValueKind::Unsigned(v) =
            inspect::update("layers/0/backing/sector_count", &new_size.to_string(), disk)
                .await
                .unwrap()
                .kind
        else {
            panic!("bad inspect value")
        };
        assert_eq!(new_size, v);
    }

    #[async_test]
    async fn test_resize() {
        const SIZE: usize = 1024 * 1024;
        const SECTORS: usize = SIZE / SECTOR_USIZE;

        let (guest_mem, mut upper) = prep_disk(SIZE).await;
        check(&guest_mem, 0, 0, SECTORS, 0);
        resize(&upper, SECTORS as u64 / 2).await;
        resize(&upper, SECTORS as u64).await;
        read(&guest_mem, &mut upper, 0, SECTORS).await;
        check(&guest_mem, 0, 0, SECTORS / 2, 0);
        for s in SECTORS / 2..SECTORS {
            let mut buf = [0u8; SECTOR_USIZE];
            guest_mem.read_at(s as u64 * SECTOR_U64, &mut buf).unwrap();
            assert_eq!(buf, [0u8; SECTOR_USIZE]);
        }
    }

    #[async_test]
    async fn test_unmap() {
        const SIZE: usize = 1024 * 1024;
        const SECTORS: usize = SIZE / SECTOR_USIZE;

        let (guest_mem, mut upper) = prep_disk(SIZE).await;
        upper.unmap(0, SECTORS as u64 - 1, false).await.unwrap();
        read(&guest_mem, &mut upper, 0, SECTORS).await;
        check(&guest_mem, 0, 0, SECTORS, 0);
        upper
            .unmap(SECTORS as u64 / 2, SECTORS as u64 / 2, false)
            .await
            .unwrap();
        read(&guest_mem, &mut upper, 0, SECTORS).await;
        check(&guest_mem, 0, 0, SECTORS / 2, 0);
        for s in SECTORS / 2..SECTORS {
            let mut buf = [0u8; SECTOR_USIZE];
            guest_mem.read_at(s as u64 * SECTOR_U64, &mut buf).unwrap();
            assert_eq!(buf, [0u8; SECTOR_USIZE]);
        }
    }

    // Test 4K sector size support
    const SECTOR_4K: u32 = 4096;

    #[async_test]
    async fn test_4k_sectors_basic() {
        // Test basic operations with 4K sectors
        const SIZE: u64 = 1024 * 1024; // 1MB

        let layer = RamDiskLayer::new_with_sector_size(SIZE, SECTOR_4K).unwrap();
        assert_eq!(layer.sector_size(), SECTOR_4K);
        assert_eq!(layer.sector_count(), SIZE / SECTOR_4K as u64);

        // Test writing and reading a 4K sector
        let guest_mem = GuestMemory::allocate(SECTOR_4K as usize);
        let test_data: Vec<u8> = (0..SECTOR_4K).map(|i| (i % 256) as u8).collect();
        guest_mem.write_at(0, &test_data).unwrap();

        layer
            .write(
                &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, false).buffer(&guest_mem),
                0,
                false,
            )
            .await
            .unwrap();

        // Clear and read back using a layered disk wrapper
        let disk = Disk::new(
            LayeredDisk::new(
                false,
                vec![LayerConfiguration {
                    layer: DiskLayer::new(layer),
                    write_through: false,
                    read_cache: false,
                }],
            )
            .await
            .unwrap(),
        )
        .unwrap();

        guest_mem.fill_at(0, 0, SECTOR_4K as usize).unwrap();
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, true).buffer(&guest_mem),
            0,
        )
        .await
        .unwrap();

        let mut read_data = vec![0u8; SECTOR_4K as usize];
        guest_mem.read_at(0, &mut read_data).unwrap();
        assert_eq!(read_data, test_data);
    }

    #[async_test]
    async fn test_4k_sectors_multiple() {
        // Test multiple 4K sectors using the convenience function
        const SIZE: u64 = 16 * 4096; // 64KB = 16 sectors

        let disk = super::ram_disk_with_sector_size(SIZE, false, SECTOR_4K).unwrap();
        let guest_mem = GuestMemory::allocate(SECTOR_4K as usize);

        // Write different patterns to different sectors
        for sector in 0..4 {
            let pattern = (sector + 1) as u8;
            let sector_data = vec![pattern; SECTOR_4K as usize];
            guest_mem.write_at(0, &sector_data).unwrap();

            disk.write_vectored(
                &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, false).buffer(&guest_mem),
                sector,
                false,
            )
            .await
            .unwrap();
        }

        // Read back and verify each sector
        for sector in 0..4 {
            guest_mem.fill_at(0, 0, SECTOR_4K as usize).unwrap();
            disk.read_vectored(
                &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, true).buffer(&guest_mem),
                sector,
            )
            .await
            .unwrap();

            let mut read_data = vec![0u8; SECTOR_4K as usize];
            guest_mem.read_at(0, &mut read_data).unwrap();
            let expected_pattern = (sector + 1) as u8;
            assert!(read_data.iter().all(|&b| b == expected_pattern));
        }
    }

    #[async_test]
    async fn test_4k_ram_disk_helper() {
        // Test the convenience function with 4K sectors
        const SIZE: u64 = 8 * 4096; // 32KB

        let disk = super::ram_disk_with_sector_size(SIZE, false, SECTOR_4K).unwrap();
        let guest_mem = GuestMemory::allocate(SECTOR_4K as usize);

        // Write a test pattern
        let test_data: Vec<u8> = (0..SECTOR_4K).map(|i| (i / 16) as u8).collect();
        guest_mem.write_at(0, &test_data).unwrap();

        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, false).buffer(&guest_mem),
            1, // Write to sector 1
            false,
        )
        .await
        .unwrap();

        // Read back
        guest_mem.fill_at(0, 0, SECTOR_4K as usize).unwrap();
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_4K as usize, true).buffer(&guest_mem),
            1,
        )
        .await
        .unwrap();

        let mut read_data = vec![0u8; SECTOR_4K as usize];
        guest_mem.read_at(0, &mut read_data).unwrap();
        assert_eq!(read_data, test_data);
    }

    #[async_test]
    async fn test_mixed_sector_sizes() {
        // Test that different layers can have different sector sizes
        let layer_512 = RamDiskLayer::new_with_sector_size(8192, 512).unwrap();
        let layer_4k = RamDiskLayer::new_with_sector_size(8192, 4096).unwrap();

        assert_eq!(layer_512.sector_size(), 512);
        assert_eq!(layer_512.sector_count(), 16); // 8192 / 512

        assert_eq!(layer_4k.sector_size(), 4096);
        assert_eq!(layer_4k.sector_count(), 2); // 8192 / 4096
    }

    #[async_test]
    async fn test_invalid_sector_sizes() {
        // Test error cases
        assert!(RamDiskLayer::new_with_sector_size(1000, 512).is_err()); // Size not multiple of sector
        assert!(RamDiskLayer::new_with_sector_size(1024, 0).is_err()); // Zero sector size
        assert!(RamDiskLayer::new_with_sector_size(0, 512).is_err()); // Zero size
    }

    #[async_test]
    async fn test_backward_compatibility() {
        // Test that the original new() method still works with 512-byte sectors
        let layer_old = RamDiskLayer::new(8192).unwrap();
        let layer_new = RamDiskLayer::new_with_sector_size(8192, 512).unwrap();

        assert_eq!(layer_old.sector_size(), layer_new.sector_size());
        assert_eq!(layer_old.sector_count(), layer_new.sector_count());

        // Test that the original ram_disk() function still works
        let disk_old = super::ram_disk(8192, false).unwrap();
        let disk_new = super::ram_disk_with_sector_size(8192, false, 512).unwrap();

        // Both should behave the same way for basic operations
        let guest_mem = GuestMemory::allocate(512);
        let test_data = vec![0x42u8; 512];
        guest_mem.write_at(0, &test_data).unwrap();

        // Write to both disks
        disk_old
            .write_vectored(
                &OwnedRequestBuffers::linear(0, 512, false).buffer(&guest_mem),
                0,
                false,
            )
            .await
            .unwrap();

        disk_new
            .write_vectored(
                &OwnedRequestBuffers::linear(0, 512, false).buffer(&guest_mem),
                0,
                false,
            )
            .await
            .unwrap();

        // Read from both and verify
        guest_mem.fill_at(0, 0, 512).unwrap();
        disk_old
            .read_vectored(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&guest_mem),
                0,
            )
            .await
            .unwrap();

        let mut data_old = vec![0u8; 512];
        guest_mem.read_at(0, &mut data_old).unwrap();

        guest_mem.fill_at(0, 0, 512).unwrap();
        disk_new
            .read_vectored(
                &OwnedRequestBuffers::linear(0, 512, true).buffer(&guest_mem),
                0,
            )
            .await
            .unwrap();

        let mut data_new = vec![0u8; 512];
        guest_mem.read_at(0, &mut data_new).unwrap();

        assert_eq!(data_old, data_new);
        assert_eq!(data_old, test_data);
    }
}
