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
            self.state.read().data.len() * size_of::<Sector>()
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

struct Sector([u8; 512]);

const SECTOR_SIZE: u32 = 512;

impl RamDiskLayer {
    /// Makes a new RAM disk layer of `size` bytes.
    pub fn new(size: u64) -> Result<Self, Error> {
        let sector_count = {
            if size == 0 {
                return Err(Error::EmptyDisk);
            }
            if !size.is_multiple_of(SECTOR_SIZE as u64) {
                return Err(Error::NotSectorMultiple {
                    disk_size: size,
                    sector_size: SECTOR_SIZE,
                });
            }
            size / SECTOR_SIZE as u64
        };
        Ok(Self {
            state: RwLock::new(RamState {
                data: BTreeMap::new(),
                sector_count,
                zero_after: sector_count,
            }),
            sector_count: sector_count.into(),
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
        let count = buffers.len() / SECTOR_SIZE as usize;
        tracing::trace!(sector, count, "write");
        let mut state = self.state.write();
        if sector + count as u64 > state.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        for i in 0..count {
            let cur = i + sector as usize;
            let buf = buffers.subrange(i * SECTOR_SIZE as usize, SECTOR_SIZE as usize);
            let mut reader = buf.reader();
            match state.data.entry(cur as u64) {
                Entry::Vacant(entry) => {
                    entry.insert(Sector(reader.read_plain()?));
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
        SECTOR_SIZE
    }

    fn is_logically_read_only(&self) -> bool {
        false
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        SECTOR_SIZE
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
        let count = (buffers.len() / SECTOR_SIZE as usize) as u64;
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
                let offset = (zero_start - sector) as usize * SECTOR_SIZE as usize;
                let len = zero_count as usize * SECTOR_SIZE as usize;
                buffers.subrange(offset, len).writer().zero(len)?;
                marker.set_range(zero_start..next);
            }
            if let Some((&s, buf)) = r {
                let offset = (s - sector) as usize * SECTOR_SIZE as usize;
                buffers
                    .subrange(offset, SECTOR_SIZE as usize)
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

/// Create a RAM disk of `size` bytes.
///
/// This is a convenience function for creating a layered disk with a single RAM
/// layer. It is useful since non-layered RAM disks are used all over the place,
/// especially in tests.
pub fn ram_disk(size: u64, read_only: bool) -> anyhow::Result<Disk> {
    use futures::future::FutureExt;

    let disk = Disk::new(
        LayeredDisk::new(
            read_only,
            vec![LayerConfiguration {
                layer: DiskLayer::new(RamDiskLayer::new(size)?),
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
    use super::RamDiskLayer;
    use super::SECTOR_SIZE;
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

    const SECTOR_U64: u64 = SECTOR_SIZE as u64;
    const SECTOR_USIZE: usize = SECTOR_SIZE as usize;

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
}
