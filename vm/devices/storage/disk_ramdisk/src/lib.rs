// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RAM-backed disk layer implementation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod resolver;

use anyhow::Context;
use disk_backend::layered::DiskLayer;
use disk_backend::layered::LayerIo;
use disk_backend::layered::LayeredDisk;
use disk_backend::layered::SectorMarker;
use disk_backend::layered::UnmapBehavior;
use disk_backend::layered::WriteNoOverwrite;
use disk_backend::zerodisk::InvalidGeometry;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::RwLock;
use scsi_buffers::RequestBuffers;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;

/// A disk backed entirely by RAM.
pub struct RamLayer {
    data: RwLock<BTreeMap<u64, Sector>>,
    sector_count: AtomicU64,
    resize_event: event_listener::Event,
}

impl Inspect for RamLayer {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field_with("committed_size", || {
                self.data.read().len() * size_of::<Sector>()
            })
            .field_mut_with("sector_count", |new_count| {
                if let Some(new_count) = new_count {
                    self.resize(new_count.parse().context("invalid sector count")?)?;
                }
                anyhow::Ok(self.sector_count())
            });
    }
}

impl Debug for RamLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RamLayer")
            .field("sector_count", &self.sector_count)
            .finish()
    }
}

/// An error creating a RAM disk.
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid disk geometry.
    #[error(transparent)]
    InvalidGeometry(#[from] InvalidGeometry),
}

struct Sector([u8; 512]);

const SECTOR_SIZE: u32 = 512;

impl RamLayer {
    /// Makes a new RAM disk of `size` bytes.
    pub fn new(size: u64) -> Result<Self, Error> {
        if size == 0 {
            return Err(Error::InvalidGeometry(InvalidGeometry::EmptyDisk));
        }
        if size % SECTOR_SIZE as u64 != 0 {
            return Err(Error::InvalidGeometry(InvalidGeometry::NotSectorMultiple {
                disk_size: size,
                sector_size: SECTOR_SIZE,
            }));
        }
        let sector_count = size / SECTOR_SIZE as u64;
        Ok(Self {
            data: RwLock::new(BTreeMap::new()),
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
            let mut data = self.data.write();
            // TODO: remember when growing the disk that missing sectors are zero.
            self.sector_count.store(new_sector_count, Ordering::Relaxed);
            data.split_off(&new_sector_count)
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
        let mut data = self.data.write();
        Ok(for i in 0..count {
            let cur = i + sector as usize;
            let buf = buffers.subrange(i * SECTOR_SIZE as usize, SECTOR_SIZE as usize);
            let mut reader = buf.reader();
            match data.entry(cur as u64) {
                Entry::Vacant(entry) => {
                    entry.insert(Sector(reader.read_plain()?));
                }
                Entry::Occupied(mut entry) => {
                    if overwrite {
                        reader.read(&mut entry.get_mut().0)?;
                    }
                }
            }
        })
    }
}

impl LayerIo for RamLayer {
    fn layer_type(&self) -> &str {
        "ram"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count.load(Ordering::Relaxed)
    }

    fn sector_size(&self) -> u32 {
        SECTOR_SIZE
    }

    fn is_read_only(&self) -> bool {
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
        mut bitmap: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        let count = (buffers.len() / SECTOR_SIZE as usize) as u64;
        tracing::trace!(sector, count, "read");
        for (&s, buf) in self.data.read().range(sector..sector + count) {
            let offset = (s - sector) as usize * SECTOR_SIZE as usize;
            buffers
                .subrange(offset, SECTOR_SIZE as usize)
                .writer()
                .write(&buf.0)?;

            bitmap.set(s);
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
        if !next_is_zero {
            return Ok(());
        }
        tracing::trace!(sector_offset, sector_count, "unmap");
        let mut data = self.data.write();
        // Sadly, there appears to be no way to remove a range of entries
        // from a btree map.
        let mut next_sector = sector_offset;
        let end = sector_offset + sector_count;
        while next_sector < end {
            let Some((&sector, _)) = data.range_mut(next_sector..).next() else {
                break;
            };
            if sector >= end {
                break;
            }
            data.remove(&sector);
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

impl WriteNoOverwrite for RamLayer {
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
    let disk = Disk::new(LayeredDisk::new(
        read_only,
        vec![DiskLayer::new(RamLayer::new(size)?, Default::default())?],
    )?)?;
    Ok(disk)
}

#[cfg(test)]
mod tests {
    use super::RamLayer;
    use super::SECTOR_SIZE;
    use disk_backend::layered::DiskLayer;
    use disk_backend::layered::LayerIo;
    use disk_backend::layered::LayeredDisk;
    use disk_backend::DiskIo;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use test_with_tracing::test;
    use zerocopy::AsBytes;

    const SECTOR_U64: u64 = SECTOR_SIZE as u64;
    const SECTOR_USIZE: usize = SECTOR_SIZE as usize;

    fn check(mem: &GuestMemory, sector: u64, start: usize, count: usize, high: u8) {
        let mut buf = vec![0u32; count * SECTOR_USIZE / 4];
        mem.read_at(start as u64 * SECTOR_U64, buf.as_bytes_mut())
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

    #[async_test]
    async fn diff() {
        const SIZE: usize = 1024 * 1024;

        let guest_mem = GuestMemory::allocate(SIZE);

        let mut lower = RamLayer::new(SIZE as u64).unwrap();
        write_layer(&guest_mem, &mut lower, 0, SIZE / SECTOR_USIZE, 0).await;
        let upper = RamLayer::new(SIZE as u64).unwrap();
        let mut upper = LayeredDisk::new(
            false,
            vec![
                DiskLayer::new(upper, Default::default()).unwrap(),
                DiskLayer::new(lower, Default::default()).unwrap(),
            ],
        )
        .unwrap();
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
}
