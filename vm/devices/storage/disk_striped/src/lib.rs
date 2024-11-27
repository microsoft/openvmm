// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the [`DiskIo`] trait for virtual disks backed by multiple raw
//! block devices.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disk_backend_resources::StripedDiskHandle;
use futures::future::join_all;
use futures::future::try_join_all;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::fmt::Debug;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;

pub struct StripedDiskResolver;
declare_static_async_resolver!(StripedDiskResolver, (DiskHandleKind, StripedDiskHandle));

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, StripedDiskHandle> for StripedDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: StripedDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disks = try_join_all(
            rsrc.devices
                .into_iter()
                .map(|device| async { resolver.resolve(device, input).await.map(|r| r.0) }),
        )
        .await?;
        Ok(ResolvedDisk::new(StripedDisk::new(
            disks,
            rsrc.chunk_size_in_bytes,
            rsrc.logic_sector_count,
        )?)?)
    }
}

#[derive(Debug, Inspect)]
pub struct StripedDisk {
    #[inspect(iter_by_index)]
    block_devices: Vec<Disk>,
    sector_size: u32,
    sector_shift: u32,
    sector_count: u64,
    read_only: bool,
    sector_count_per_chunk: u32,
    unmap_behavior: UnmapBehavior,
}

const CHUNK_SIZE_128K: u32 = 128 * 1024;

#[derive(Error, Debug)]
pub enum NewDeviceError {
    #[error("Can't create a striping disk since the input device list is empty")]
    EmptyDeviceList,
    #[error("The files are not compatible to form a striping disk: sector_size-{sector_size} != cur_sector_size-{cur_sector_size} OR sector_count-{sector_count} != cur_sector_count-{cur_sector_count}")]
    DeviceNotCompatible {
        sector_size: u32,
        cur_sector_size: u32,
        sector_count: u64,
        cur_sector_count: u64,
    },
    #[error(
        "Invalid chunk size: chunk_size_in_bytes-{0} is not multiple of logical_sector_size-{1}"
    )]
    InvalidChunkSize(u32, u32),
    #[error(
        "logic_sector_count is out of range: logic_sector_count.unwrap_or(total_sector_count)-{0} > total_sector_count-{1}"
    )]
    InvalidLogicSectorCount(u64, u64),
    #[error(
        "The striping disk size must be multiple of chunk size * number of disks. logic_sector_count-{0} != {1}."
    )]
    InvalidStripingDiskSize(u64, u64),
}

#[derive(Debug, Error)]
enum IoError {
    #[error("cur_buf_offset-{cur_buf_offset} != buf_total_size -{buf_total_size}")]
    InternalErrorBufferLengthMismatch {
        cur_buf_offset: usize,
        buf_total_size: usize,
    },
    #[error("trimmed_sectors-{trimmed_sectors} != sector_count -{sector_count}")]
    InternalErrorTrimLengthMismatch {
        trimmed_sectors: u64,
        sector_count: u64,
    },
    #[error(
        "Sector out of range: start_sector-{start_sector}, end_sector-{end_sector}, self.sector_count-{disk_sector_count}"
    )]
    IOInvalidSector {
        start_sector: u64,
        end_sector: u64,
        disk_sector_count: u64,
    },
    #[error("error in lower disk {index}")]
    LowerError {
        index: usize,
        #[source]
        err: DiskError,
    },
}

impl From<IoError> for DiskError {
    fn from(err: IoError) -> Self {
        DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, err))
    }
}

struct Chunk {
    // The index of the disk where the chunk is in.
    disk_index: usize,
    // The chunk starting sector and offset on the disk.
    disk_sector_index: u64,
    // The chunk length. It can be less the sector_count_per_chunk for the first and last chunk.
    chunk_length_in_sectors: u32,
}

#[derive(Debug, Clone)]
struct ChunkIter {
    disk_count: usize,
    sector_count_per_chunk: u32,
    start_sector: u64,
    end_sector: u64,
    start_chunk_index: u64,
    end_chunk_index: u64,
    cur_chunk_index: u64,
}

impl Iterator for ChunkIter {
    type Item = Chunk;

    fn next(&mut self) -> Option<Chunk> {
        // The valid range is [start_chunk_index, end_chunk_index).
        assert!(
            self.cur_chunk_index >= self.start_chunk_index,
            "self.cur_chunk_index-[{}] < self.start_chunk_index-[{}] should never happen.",
            self.cur_chunk_index,
            self.start_chunk_index
        );

        if self.cur_chunk_index >= self.end_chunk_index {
            return None;
        }

        // The sector can be in middle of a chunk for the first chunk.
        let sector_offset_in_chunk = if self.cur_chunk_index == self.start_chunk_index {
            self.start_sector % self.sector_count_per_chunk as u64
        } else {
            0
        };

        let disk_index = (self.cur_chunk_index % (self.disk_count as u64)) as usize;
        let disk_sector_index = (self.cur_chunk_index / self.disk_count as u64)
            * self.sector_count_per_chunk as u64
            + sector_offset_in_chunk;

        // The disk end offset can be in middle of the chunk for the last chunk.
        let disk_end_offset_in_sectors = (self.cur_chunk_index / self.disk_count as u64)
            * self.sector_count_per_chunk as u64
            + if self.cur_chunk_index == self.end_chunk_index - 1 {
                self.end_sector - self.sector_count_per_chunk as u64 * self.cur_chunk_index
            } else {
                self.sector_count_per_chunk as u64
            };

        // The chunk length can be less the sector_count_per_chunk for the first and last chunk.
        let chunk_length_in_sectors = (disk_end_offset_in_sectors - disk_sector_index) as u32;

        self.cur_chunk_index += 1;

        Some(Chunk {
            disk_index,
            disk_sector_index,
            chunk_length_in_sectors,
        })
    }
}

impl StripedDisk {
    fn get_chunk_iter(&self, start_sector: u64, end_sector: u64) -> Result<ChunkIter, DiskError> {
        // The valid range is [start_sector, end_sector).
        if end_sector > self.sector_count {
            let err = IoError::IOInvalidSector {
                start_sector,
                end_sector,
                disk_sector_count: self.sector_count,
            };
            tracelimit::error_ratelimited!(err = ?err);
            return Err(err.into());
        }

        let start_chunk_index = start_sector / self.sector_count_per_chunk as u64;
        let end_chunk_index = (end_sector + self.sector_count_per_chunk as u64 - 1)
            / self.sector_count_per_chunk as u64;

        let chunk_iter = ChunkIter {
            disk_count: self.block_devices.len(),
            sector_count_per_chunk: self.sector_count_per_chunk,
            start_sector,
            end_sector,
            start_chunk_index,
            end_chunk_index,
            cur_chunk_index: start_chunk_index,
        };

        Ok(chunk_iter)
    }
}

impl StripedDisk {
    /// Constructs a new `StripedDisk` backed by the vector of file.
    ///
    /// # Arguments
    /// * `devices` - The backing devices opened for raw access.
    /// * 'chunk_size_in_bytes' - The chunk size of the striped disk, and the default value is 128K.
    /// * 'logic_sector_count' - The sector count of the striped disk, and the default value is the sum of the sector count of the backing devices.
    ///
    pub fn new(
        devices: Vec<Disk>,
        chunk_size_in_bytes: Option<u32>,
        logic_sector_count: Option<u64>,
    ) -> Result<Self, NewDeviceError> {
        if devices.is_empty() {
            return Err(NewDeviceError::EmptyDeviceList);
        }

        let mut total_sector_count = 0;
        let sector_size = devices[0].sector_size();
        let sector_count = devices[0].sector_count();
        let read_only = devices[0].is_read_only();
        let chunk_size_in_bytes = chunk_size_in_bytes.unwrap_or(CHUNK_SIZE_128K);
        if chunk_size_in_bytes % sector_size != 0 {
            return Err(NewDeviceError::InvalidChunkSize(
                chunk_size_in_bytes,
                sector_size,
            ));
        }

        let sector_count_per_chunk = (chunk_size_in_bytes / sector_size) as u64;

        for device in &devices {
            let cur_sector_size = device.sector_size();
            let cur_sector_count = device.sector_count();
            let cur_read_only = device.is_read_only();

            if sector_size != cur_sector_size
                || sector_count != cur_sector_count
                || read_only != cur_read_only
            {
                return Err(NewDeviceError::DeviceNotCompatible {
                    sector_size,
                    cur_sector_size,
                    sector_count,
                    cur_sector_count,
                });
            }

            total_sector_count +=
                (cur_sector_count / sector_count_per_chunk) * sector_count_per_chunk;
        }

        if total_sector_count % (devices.len() as u64 * sector_count_per_chunk) != 0 {
            return Err(NewDeviceError::InvalidStripingDiskSize(
                total_sector_count,
                devices.len() as u64 * sector_count_per_chunk,
            ));
        }

        let logic_sector_count = logic_sector_count.unwrap_or(total_sector_count);
        if logic_sector_count > total_sector_count {
            return Err(NewDeviceError::InvalidLogicSectorCount(
                logic_sector_count,
                total_sector_count,
            ));
        }

        if logic_sector_count % (devices.len() as u64 * sector_count_per_chunk) != 0 {
            return Err(NewDeviceError::InvalidStripingDiskSize(
                logic_sector_count,
                devices.len() as u64 * sector_count_per_chunk,
            ));
        }

        // Unify the unmap behavior of all devices. If all disks specify the
        // same behavior, use it. Otherwise, report unspecified behavior and
        // send unmap to all disks.
        let unmap_behavior = devices.iter().fold(UnmapBehavior::Zeroes, |rest, d| {
            match (rest, d.unmap_behavior()) {
                (UnmapBehavior::Zeroes, UnmapBehavior::Zeroes) => UnmapBehavior::Zeroes,
                (UnmapBehavior::Ignored, UnmapBehavior::Ignored) => UnmapBehavior::Ignored,
                _ => UnmapBehavior::Unspecified,
            }
        });

        let stripped_block_device = StripedDisk {
            block_devices: devices,
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            sector_count: logic_sector_count,
            read_only,
            sector_count_per_chunk: (sector_count_per_chunk as u32),
            unmap_behavior,
        };

        tracing::info!("stripped block device start completed.");
        Ok(stripped_block_device)
    }
}

impl DiskIo for StripedDisk {
    fn disk_type(&self) -> &str {
        "striped"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.block_devices
            .iter()
            .map(|d| d.physical_sector_size())
            .max()
            .unwrap()
    }

    fn is_fua_respected(&self) -> bool {
        self.block_devices.iter().all(|d| d.is_fua_respected())
    }

    async fn eject(&self) -> Result<(), DiskError> {
        let mut futures = Vec::new();
        for disk in &self.block_devices {
            futures.push(disk.eject());
        }
        await_all_and_check(futures).await?;
        Ok(())
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        start_sector: u64,
    ) -> Result<(), DiskError> {
        let buf_total_size = buffers.len();
        let end_sector = start_sector + ((buf_total_size as u64) >> self.sector_shift);
        if end_sector > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        let chunk_iter = self.get_chunk_iter(start_sector, end_sector)?;

        let mut all_futures = Vec::new();
        let mut cur_buf_offset: usize = 0;

        for chunk in chunk_iter {
            let disk = &self.block_devices[chunk.disk_index];

            let buf_len = (chunk.chunk_length_in_sectors as usize) << self.sector_shift;

            let sub_buffers = buffers.subrange(cur_buf_offset, buf_len);
            cur_buf_offset += buf_len;

            all_futures.push(async move {
                disk.read_vectored(&sub_buffers, chunk.disk_sector_index)
                    .await
                    .map_err(|err| IoError::LowerError {
                        index: chunk.disk_index,
                        err,
                    })
            });
        }

        if cur_buf_offset != buf_total_size {
            return Err(IoError::InternalErrorBufferLengthMismatch {
                cur_buf_offset,
                buf_total_size,
            }
            .into());
        }

        await_all_and_check(all_futures).await?;
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        start_sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let buf_total_size = buffers.len();
        let end_sector = start_sector + ((buf_total_size as u64) >> self.sector_shift);
        if end_sector > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        let chunk_iter = self.get_chunk_iter(start_sector, end_sector)?;

        let mut all_futures = Vec::new();
        let mut cur_buf_offset: usize = 0;

        for chunk in chunk_iter {
            let disk = &self.block_devices[chunk.disk_index];

            let buf_len = (chunk.chunk_length_in_sectors as usize) << self.sector_shift;

            let sub_buffers = buffers.subrange(cur_buf_offset, buf_len);
            cur_buf_offset += buf_len;

            all_futures.push(async move {
                disk.write_vectored(&sub_buffers, chunk.disk_sector_index, fua)
                    .await
                    .map_err(|err| IoError::LowerError {
                        index: chunk.disk_index,
                        err,
                    })
            });
        }

        if cur_buf_offset != buf_total_size {
            return Err(IoError::InternalErrorBufferLengthMismatch {
                cur_buf_offset,
                buf_total_size,
            }
            .into());
        }

        await_all_and_check(all_futures).await?;
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        let mut all_futures = Vec::new();
        for (disk_index, disk) in self.block_devices.iter().enumerate() {
            all_futures.push(async move {
                disk.sync_cache().await.map_err(|err| IoError::LowerError {
                    index: disk_index,
                    err,
                })
            });
        }
        await_all_and_check(all_futures).await?;
        Ok(())
    }

    async fn unmap(
        &self,
        start_sector: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        let end_sector = start_sector + sector_count;

        if end_sector > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }

        let chunk_iter = match self.get_chunk_iter(start_sector, end_sector) {
            Ok(iter) => iter,
            Err(err) => {
                return Err(err);
            }
        };

        // Create a vector to group chunks by disk index
        let mut disk_sectors: Vec<(u64, u64)> = vec![(0, 0); self.block_devices.len()];
        let mut trimmed_sectors: u64 = 0;

        for chunk in chunk_iter {
            let start = chunk.disk_sector_index;
            let length = chunk.chunk_length_in_sectors as u64;
            let (disk_start, disk_len) = &mut disk_sectors[chunk.disk_index];
            if *disk_len == 0 {
                *disk_start = start; // set the start of the unmap operation
            }
            *disk_len += length; // add the length to the total

            trimmed_sectors += length;
        }

        if trimmed_sectors != sector_count {
            return Err(IoError::InternalErrorTrimLengthMismatch {
                trimmed_sectors,
                sector_count,
            }
            .into());
        }

        // Create a future for each disk's combined unmap operations
        let mut all_futures = Vec::new();

        for (disk_index, &(start, length)) in disk_sectors.iter().enumerate() {
            let disk = &self.block_devices[disk_index];
            // Check if the length is non-zero before pushing to all_futures
            if length > 0 {
                all_futures.push(async move { disk.unmap(start, length, block_level_only).await });
            }
        }
        await_all_and_check(all_futures).await?;
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.unmap_behavior
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.block_devices
            .iter()
            .map(|disk| disk.optimal_unmap_sectors())
            .max()
            .unwrap_or(1)
    }
}

async fn await_all_and_check<T, E>(futures: T) -> Result<(), E>
where
    T: IntoIterator,
    T::Item: core::future::Future<Output = Result<(), E>>,
{
    // Use join_all to wait for all IOs even if one fails. This is necessary to
    // avoid dropping IOs while they are in flight.
    let results = join_all(futures).await;
    for result in results {
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::GuestMemory;
    use hvdef::HV_PAGE_SIZE;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;

    fn new_strip_device(
        disk_count: u8,
        disk_size_in_bytes: Option<u64>,
        chunk_size_in_bytes: Option<u32>,
        logic_sector_count: Option<u64>,
    ) -> StripedDisk {
        let mut devices = Vec::new();

        for _i in 0..disk_count {
            let ramdisk =
                disk_ramdisk::ram_disk(disk_size_in_bytes.unwrap_or(1024 * 1024 * 64), false)
                    .unwrap();
            devices.push(ramdisk);
        }

        StripedDisk::new(devices, chunk_size_in_bytes, logic_sector_count).unwrap()
    }

    fn create_guest_mem(size: usize) -> GuestMemory {
        let mem = GuestMemory::allocate(size);

        let mut index: usize = 0;
        while index < size - 3 {
            mem.write_at(
                index as u64,
                &[
                    (index % 255) as u8,
                    ((index >> 8) % 255) as u8,
                    ((index >> 16) % 255) as u8,
                    ((index >> 24) % 255) as u8,
                ],
            )
            .unwrap();

            index += 4;
        }

        mem
    }

    async fn validate_async_striping_disk_ios(
        disk: &StripedDisk,
        start_sectors: &[u64],
        offset: &[usize],
        length: usize,
        write_gpns: &[u64],
        read_gpns: &[u64],
    ) {
        for (start_sector, offset) in start_sectors.iter().zip(offset) {
            validate_async_striping_disk_io(
                disk,
                *start_sector,
                *offset,
                length,
                write_gpns,
                read_gpns,
            )
            .await;
        }
    }

    /// Validate the async strip disk I/O.
    ///
    /// # Arguments
    /// * `disk` - The strip block device.
    /// * `start_sector` - The sector index where the I/O shall start.
    /// * `offset` - The I/O buffer offset.
    /// * `length` - The total I/O length.
    /// * `write_gpns` - The write GPN index.
    /// * `read_gpns` - The read GPN index.
    ///
    async fn validate_async_striping_disk_io(
        disk: &StripedDisk,
        start_sector: u64,
        offset: usize,
        length: usize,
        write_gpns: &[u64],
        read_gpns: &[u64],
    ) {
        let page_count = (offset + length + HV_PAGE_SIZE as usize - 1) / HV_PAGE_SIZE as usize;
        // Create continuous guest memory pages and initialize them with random data.
        let guest_mem = create_guest_mem(page_count * 2 * HV_PAGE_SIZE as usize);
        assert_eq!(write_gpns.len(), page_count);
        assert_eq!(read_gpns.len(), page_count);

        // Get the write buffer from guest memory, which has random data.
        let write_buffers = OwnedRequestBuffers::new_unaligned(write_gpns, offset, length);
        // Write the random data to disk.
        disk.write_vectored(&write_buffers.buffer(&guest_mem), start_sector, false)
            .await
            .unwrap();

        disk.sync_cache().await.unwrap();

        // Get the read buffer from guest memory, which has random data.
        let read_buffers = OwnedRequestBuffers::new_unaligned(read_gpns, offset, length);
        // Read the data from disk back to read buffers.
        disk.read_vectored(&read_buffers.buffer(&guest_mem), start_sector)
            .await
            .unwrap();

        // Validate if the source and target match.
        let mut source = vec![0u8; page_count * HV_PAGE_SIZE as usize];
        guest_mem.read_at(0, &mut source).unwrap();

        let mut target = vec![255u8; page_count * HV_PAGE_SIZE as usize];
        guest_mem
            .read_at(page_count as u64 * HV_PAGE_SIZE, &mut target)
            .unwrap();

        assert_eq!(
            source[offset..(offset + length - 1)],
            target[offset..(offset + length - 1)]
        );

        // async_trim test
        // Since the discard function doesn't trim the file content, the test doesn't check if the file content is ZERO after the trim.
        disk.unmap(
            start_sector,
            (length / disk.sector_size() as usize) as u64,
            true,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn run_async_striping_disk_io() {
        // Create a striping disk with two disks, set the chunk size to 4K and total size to 256K.
        let disk = new_strip_device(2, Some(128 * 1024), Some(4096), None);
        assert_eq!(disk.sector_size, 512);
        assert_eq!(disk.sector_count_per_chunk, 4096 / 512);
        assert_eq!(disk.sector_count(), 128 * 1024 * 2 / 512);

        // Read 1K data from the beginning, middle, and end of the disk using paged aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 2],
            &[0, 0, 0],
            1024,
            &[0],
            &[1],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 2],
            &[0, 0, 0],
            512,
            &[0],
            &[1],
        )
        .await;

        // Read 16K data from the beginning, middle, and end of the disk using paged aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 16, disk.sector_count() - 32],
            &[0, 0, 0],
            16 * 1024,
            &[0, 1, 2, 3],
            &[4, 5, 6, 7],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 4],
            &[512, 513, 1028],
            512,
            &[0],
            &[1],
        )
        .await;

        // Read 5K data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 5, disk.sector_count() - 10],
            &[512, 513, 1028],
            5 * 1024,
            &[0, 1],
            &[2, 3],
        )
        .await;
    }

    #[async_test]
    async fn run_async_128k_striping_disk_io() {
        // Create a striping disk with four disks, set the chunk size to 128K and total size to 4M.
        let disk = new_strip_device(4, Some(1024 * 1024), Some(128 * 1024), None);
        assert_eq!(disk.sector_size, 512);
        assert_eq!(disk.sector_count_per_chunk, 128 * 1024 / 512);
        assert_eq!(disk.sector_count(), 1024 * 1024 * 4 / 512);

        // Read 1K data from the beginning, middle, and end of the disk using paged aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 2],
            &[0, 0, 0],
            1024,
            &[0],
            &[1],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 2],
            &[0, 0, 0],
            512,
            &[0],
            &[1],
        )
        .await;

        // Read 256K data from the beginning, middle, and end of the disk using paged aligned buffers.
        let mut write_gpns: [u64; 256 * 1024 / HV_PAGE_SIZE as usize] =
            [0; 256 * 1024 / HV_PAGE_SIZE as usize];
        for (i, write_gpn) in write_gpns.iter_mut().enumerate() {
            *write_gpn = i as u64;
        }

        let mut read_gpns: [u64; 256 * 1024 / HV_PAGE_SIZE as usize] =
            [0; 256 * 1024 / HV_PAGE_SIZE as usize];
        for (i, read_gpn) in read_gpns.iter_mut().enumerate() {
            *read_gpn = (i + write_gpns.len()) as u64;
        }

        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 256, disk.sector_count() - 512],
            &[0, 0, 0],
            256 * 1024,
            &write_gpns,
            &read_gpns,
        )
        .await;

        // Read 9K data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 9, disk.sector_count() - 18],
            &[512, 513, 1028],
            9 * 1024,
            &[0, 1, 2],
            &[3, 4, 5],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 4],
            &[512, 513, 1028],
            512,
            &[0],
            &[1],
        )
        .await;
    }

    #[async_test]
    async fn run_async_64k_striping_disk_io() {
        // Create a striping disk with thirty two disks, set the chunk size to 64K and total size to 32M.
        let disk = new_strip_device(32, Some(1024 * 1024), Some(64 * 1024), None);
        assert_eq!(disk.sector_size, 512);
        assert_eq!(disk.sector_count_per_chunk, 64 * 1024 / 512);
        assert_eq!(disk.sector_count(), 1024 * 1024 * 32 / 512);

        // Read 1K data from the beginning, middle, and end of the disk using paged aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 2],
            &[0, 0, 0],
            1024,
            &[0],
            &[1],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 1],
            &[0, 0, 0],
            512,
            &[0],
            &[1],
        )
        .await;

        // Read 256K data from the beginning, middle, and end of the disk using paged aligned buffers.
        let mut write_gpns: [u64; 256 * 1024 / HV_PAGE_SIZE as usize] =
            [0; 256 * 1024 / HV_PAGE_SIZE as usize];
        for (i, write_gpn) in write_gpns.iter_mut().enumerate() {
            *write_gpn = i as u64;
        }

        let mut read_gpns: [u64; 256 * 1024 / HV_PAGE_SIZE as usize] =
            [0; 256 * 1024 / HV_PAGE_SIZE as usize];
        for (i, read_gpn) in read_gpns.iter_mut().enumerate() {
            *read_gpn = (i + write_gpns.len()) as u64;
        }

        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 256, disk.sector_count() - 512],
            &[0, 0, 0],
            256 * 1024,
            &write_gpns,
            &read_gpns,
        )
        .await;

        // Read 9K data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 9, disk.sector_count() - 18],
            &[512, 513, 1028],
            9 * 1024,
            &[0, 1, 2],
            &[3, 4, 5],
        )
        .await;

        // Read 512 bytes data from the beginning, middle, and end of the disk using un-aligned buffers.
        validate_async_striping_disk_ios(
            &disk,
            &[0, disk.sector_count() / 2 - 1, disk.sector_count() - 4],
            &[512, 513, 1028],
            512,
            &[0],
            &[1],
        )
        .await;
    }

    #[async_test]
    async fn run_async_striping_disk_negative() {
        // Creating striping disk using incompatible files shall fail.
        let mut devices = Vec::new();
        for i in 0..2 {
            let ramdisk = disk_ramdisk::ram_disk(1024 * 1024 + i * 64 * 1024, false).unwrap();
            devices.push(ramdisk);
        }

        match StripedDisk::new(devices, None, None) {
            Err(err) => {
                println!(
                    "Expected failure since underlying files are not compatible: {}",
                    err
                );
            }
            Ok(strip_disk) => panic!("{:?}", strip_disk),
        }

        // Creating striping disk using invalid chunk size shall fail.
        let mut block_devices = Vec::new();
        for _ in 0..2 {
            let ramdisk = disk_ramdisk::ram_disk(1024 * 1024, false).unwrap();
            block_devices.push(ramdisk);
        }

        match StripedDisk::new(block_devices, Some(4 * 1024 + 1), None) {
            Err(err) => {
                println!("Expected failure since chunk size is invalid: {}", err);
            }
            Ok(strip_disk) => panic!("{:?}", strip_disk),
        }

        // Creating striping disk using invalid logic sector count shall fail.
        let mut block_devices = Vec::new();
        for _ in 0..2 {
            let ramdisk = disk_ramdisk::ram_disk(1024 * 1024, false).unwrap();
            block_devices.push(ramdisk);
        }

        match StripedDisk::new(
            block_devices,
            Some(4 * 1024),
            Some(1024 * 1024 * 2 / 512 + 1),
        ) {
            Err(err) => {
                println!(
                    "Expected failure since logic sector count is invalid: {}",
                    err
                );
            }
            Ok(strip_disk) => panic!("{:?}", strip_disk),
        }

        // Create a simple striping disk.
        let mut block_devices = Vec::new();
        for _ in 0..2 {
            let ramdisk = disk_ramdisk::ram_disk(1024 * 1024, false).unwrap();
            block_devices.push(ramdisk);
        }

        let disk = match StripedDisk::new(block_devices, Some(8 * 1024), None) {
            Err(err) => panic!("{}", err),
            Ok(strip_disk) => strip_disk,
        };

        assert_eq!(disk.sector_size, 512);
        assert_eq!(disk.sector_count_per_chunk, 8 * 1024 / 512);
        assert_eq!(disk.sector_count(), 1024 * 1024 * 2 / 512);

        // write 1 sector off shall be caught.
        let guest_mem = create_guest_mem(2 * HV_PAGE_SIZE as usize);
        let write_buffers = OwnedRequestBuffers::new(&[0]);
        let buf_sector_count =
            (write_buffers.len() + disk.sector_size as usize - 1) / disk.sector_size as usize;
        match disk
            .write_vectored(
                &write_buffers.buffer(&guest_mem),
                disk.sector_count() - buf_sector_count as u64 + 1,
                false,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected write failure because of 1 sector off: {:?}", err);
            }
        }

        // read 1 sector off shall be caught.
        let guest_mem = create_guest_mem(2 * HV_PAGE_SIZE as usize);
        let read_buffers = OwnedRequestBuffers::new(&[1]);
        let buf_sector_count =
            (read_buffers.len() + disk.sector_size as usize - 1) / disk.sector_size as usize;
        match disk
            .read_vectored(
                &write_buffers.buffer(&guest_mem),
                disk.sector_count() - buf_sector_count as u64 + 1,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected read failure because of 1 sector off: {:?}", err);
            }
        }

        match disk
            .unmap(
                (disk.sector_count() - 2) * disk.sector_size as u64,
                disk.sector_size as u64 * 3,
                true,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected failure because of 1 sector off: {:?}", err);
            }
        }

        // write 1 byte off shall be caught.
        let write_buffers =
            OwnedRequestBuffers::new_unaligned(&[0], 0, disk.sector_size as usize + 1);
        let buf_sector_count =
            (write_buffers.len() + disk.sector_size as usize - 1) / disk.sector_size as usize;
        match disk
            .write_vectored(
                &write_buffers.buffer(&guest_mem),
                disk.sector_count() - buf_sector_count as u64 + 1,
                false,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected failure because of write 1 byte off: {:?}", err);
            }
        }

        // read 1 byte off shall be caught.
        let read_buffers =
            OwnedRequestBuffers::new_unaligned(&[1], 0, disk.sector_size as usize + 1);
        let buf_sector_count =
            (read_buffers.len() + disk.sector_size as usize - 1) / disk.sector_size as usize;
        match disk
            .read_vectored(
                &read_buffers.buffer(&guest_mem),
                disk.sector_count() - buf_sector_count as u64 + 1,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected failure because of read 1 byte off: {:?}", err);
            }
        }

        match disk
            .unmap(
                (disk.sector_count() - 2) * disk.sector_size as u64,
                disk.sector_size as u64 * 2 + 1,
                true,
            )
            .await
        {
            Ok(_) => {
                panic!("{:?}", disk);
            }
            Err(err) => {
                println!("Expected failure because of 1 byte off: {:?}", err);
            }
        }
    }

    #[async_test]
    async fn run_async_striping_disk_unmap() {
        let disk = new_strip_device(2, Some(128 * 1024 * 1024), Some(4096), None);
        assert_eq!(disk.sector_size, 512);
        assert_eq!(disk.sector_count_per_chunk, 4096 / 512);
        assert_eq!(disk.sector_count(), 128 * 1024 * 1024 * 2 / 512); //sector_count =  524288
        disk.unmap(0, 1, false).await.unwrap();
        disk.unmap(0, 524288, false).await.unwrap();
        disk.unmap(8, 524280, false).await.unwrap();
        disk.unmap(disk.sector_count() / 2 - 512, 1024, false)
            .await
            .unwrap();
        disk.unmap(disk.sector_count() - 1024, 1024, false)
            .await
            .unwrap();
        disk.unmap(0, disk.sector_count() / 2, false).await.unwrap();
        disk.unmap(disk.sector_count() / 2, disk.sector_count() / 2, false)
            .await
            .unwrap();
        disk.unmap(disk.sector_count() / 2 - 500, 1000, false)
            .await
            .unwrap();
        //this one should fail, out of bounds
        assert!(disk.unmap(disk.sector_count(), 100, false).await.is_err());
        //unmap zero sector
        disk.unmap(1000, 0, false).await.unwrap();
    }
}
