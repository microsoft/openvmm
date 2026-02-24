// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio block device implementation.

#![forbid(unsafe_code)]

pub mod resolver;
mod spec;

use crate::spec::*;
use disk_backend::Disk;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use pal_async::task::Spawn;
use pal_async::wait::PolledWait;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::VirtioDeviceFeaturesBank0;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const PAGE_SIZE: u64 = 4096;
const MAX_IO_DEPTH: usize = 64;

/// The virtio-blk device.
#[derive(InspectMut)]
pub struct Device {
    disk: Disk,
    #[inspect(skip)]
    memory: GuestMemory,
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    driver: VmTaskDriver,
    read_only: bool,
    #[inspect(skip)]
    config: VirtioBlkConfig,
    worker: Option<Worker>,
}

struct Worker {
    task: TaskControl<WorkerTask, WorkerState>,
}

impl Inspect for Worker {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task.inspect(req);
    }
}

#[derive(Inspect)]
struct WorkerState {
    disk: Disk,
    #[inspect(skip)]
    memory: GuestMemory,
    read_only: bool,
    #[inspect(flatten)]
    stats: WorkerStats,
}

#[derive(Inspect, Default)]
struct WorkerStats {
    read_ops: Counter,
    write_ops: Counter,
    flush_ops: Counter,
    discard_ops: Counter,
    errors: Counter,
}

struct WorkerTask {
    queue: VirtioQueue,
}

impl InspectTask<WorkerState> for WorkerTask {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&WorkerState>) {
        if let Some(state) = state {
            state.inspect(req);
        }
    }
}

/// Result of a completed IO operation, returned from the spawned future
/// back to the main task for stats accumulation and descriptor completion.
struct IoCompletion {
    #[allow(dead_code)]
    work: VirtioQueueCallbackWork,
    #[allow(dead_code)]
    bytes_written: u32,
    stat: IoStat,
}

/// Which stat counter to increment for a completed IO.
enum IoStat {
    Read,
    Write,
    Flush,
    Discard,
    Error,
    None,
}

impl AsyncRun<WorkerState> for WorkerTask {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut WorkerState,
    ) -> Result<(), task_control::Cancelled> {
        let mut ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoCompletion> + Send>>> =
            FuturesUnordered::new();

        stop.until_stopped(async {
            loop {
                enum Event {
                    NewWork(Result<VirtioQueueCallbackWork, std::io::Error>),
                    Completed(IoCompletion),
                }

                let event = std::future::poll_fn(|cx| {
                    // Poll for completed IOs first to free up slots.
                    if let Poll::Ready(Some(completion)) = ios.poll_next_unpin(cx) {
                        return Poll::Ready(Event::Completed(completion));
                    }
                    // Accept new work if under the depth limit.
                    if ios.len() < MAX_IO_DEPTH {
                        if let Poll::Ready(item) = self.queue.poll_next_unpin(cx) {
                            let item = item.expect("virtio queue stream never ends");
                            return Poll::Ready(Event::NewWork(item));
                        }
                    }
                    Poll::Pending
                })
                .await;

                match event {
                    Event::NewWork(Ok(work)) => {
                        let disk = state.disk.clone();
                        let mem = state.memory.clone();
                        let read_only = state.read_only;
                        ios.push(Box::pin(async move {
                            process_request(&disk, &mem, read_only, work).await
                        }));
                    }
                    Event::NewWork(Err(err)) => {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "error reading from virtio queue"
                        );
                    }
                    Event::Completed(completion) => {
                        match completion.stat {
                            IoStat::Read => state.stats.read_ops.increment(),
                            IoStat::Write => state.stats.write_ops.increment(),
                            IoStat::Flush => state.stats.flush_ops.increment(),
                            IoStat::Discard => state.stats.discard_ops.increment(),
                            IoStat::Error => state.stats.errors.increment(),
                            IoStat::None => {}
                        }
                        // work is completed inside process_request
                        drop(completion);
                    }
                }
            }
        })
        .await
    }
}

impl Device {
    /// Creates a new virtio-blk device backed by the given disk.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        memory: GuestMemory,
        disk: Disk,
        read_only: bool,
    ) -> Self {
        let sector_count = disk.sector_count();
        let sector_size = disk.sector_size();
        let physical_sector_size = disk.physical_sector_size();

        let physical_block_exp = if physical_sector_size > sector_size {
            (physical_sector_size / sector_size).trailing_zeros() as u8
        } else {
            0
        };

        // Virtio block config space (spec §5.2.4).
        //
        // `capacity` is always present. Other fields are gated by feature bits
        // we advertise in `traits()`.
        let config = VirtioBlkConfig {
            // Capacity in 512-byte sectors (spec §5.2.4). The protocol always
            // uses 512-byte units regardless of the disk's native sector size.
            capacity: sector_count * (sector_size as u64 / 512),
            // Maximum bytes in a single segment (VIRTIO_BLK_F_SIZE_MAX).
            // 4 MiB keeps individual DMA mappings manageable.
            size_max: DEFAULT_SIZE_MAX,
            // Maximum segments per request (VIRTIO_BLK_F_SEG_MAX).
            // 128 segments × 4 MiB = 512 MiB max per request.
            seg_max: DEFAULT_SEG_MAX,
            // CHS geometry (VIRTIO_BLK_F_GEOMETRY) — not advertised, zeroed.
            geometry: VirtioBlkGeometry {
                cylinders: 0,
                heads: 0,
                sectors: 0,
            },
            // Native logical block size (VIRTIO_BLK_F_BLK_SIZE, spec §5.2.5).
            // Doesn't change protocol units but lets the driver align I/O.
            blk_size: sector_size,
            // Topology (VIRTIO_BLK_F_TOPOLOGY, spec §5.2.5 step 4).
            topology: VirtioBlkTopology {
                physical_block_exp,
                alignment_offset: 0,
                // Suggested minimum I/O size in logical blocks. 1 = no constraint.
                min_io_size: 1,
                // Optimal (max) I/O size in logical blocks. 0 = no hint.
                opt_io_size: 0,
            },
            // We don't advertise CONFIG_WCE; set writeback=1 to indicate
            // writeback cache semantics (driver should use FLUSH).
            writeback: 1,
            unused0: 0,
            // We don't advertise MQ; single queue.
            num_queues: 1,
            // Discard fields (VIRTIO_BLK_F_DISCARD, spec §5.2.4).
            // u32::MAX × 512 bytes ≈ 2 TiB per segment; no practical limit.
            max_discard_sectors: u32::MAX,
            max_discard_seg: 1,
            // Alignment in 512-byte sectors for discard ranges. Uses the
            // backend's optimal unmap granularity (same as SCSI Optimal
            // Unmap Granularity), converted to 512-byte units.
            discard_sector_alignment: disk.optimal_unmap_sectors()
                * (sector_size / 512),
            // Write zeroes fields (VIRTIO_BLK_F_WRITE_ZEROES) — not advertised.
            max_write_zeroes_sectors: 0,
            max_write_zeroes_seg: 0,
            write_zeroes_may_unmap: 0,
            unused1: [0; 3],
            _padding: [0; 4],
        };

        Self {
            disk,
            memory,
            driver_source: driver_source.clone(),
            driver: driver_source.simple(),
            read_only,
            config,
            worker: None,
        }
    }
}

impl VirtioDevice for Device {
    fn traits(&self) -> DeviceTraits {
        let mut features = VIRTIO_BLK_F_SIZE_MAX
            | VIRTIO_BLK_F_SEG_MAX
            | VIRTIO_BLK_F_BLK_SIZE
            | VIRTIO_BLK_F_FLUSH
            | VIRTIO_BLK_F_TOPOLOGY;

        if self.read_only {
            features |= VIRTIO_BLK_F_RO;
        }
        if self.disk.unmap_behavior() != disk_backend::UnmapBehavior::Ignored {
            features |= VIRTIO_BLK_F_DISCARD;
            // FUTURE: investigate adding VIRTIO_BLK_F_WRITE_ZEROES support
            // by adding an explicit write_zeroes operation to the DiskIo
            // backend trait, rather than emulating it with bounce-buffer writes.
        }

        DeviceTraits {
            device_id: VIRTIO_BLK_DEVICE_ID,
            device_features: VirtioDeviceFeatures::new()
                .with_bank0(VirtioDeviceFeaturesBank0::new().with_device_specific(features)),
            max_queues: 1,
            // Config space is 60 bytes (size_of minus 4 bytes of struct padding).
            device_register_length: (size_of::<VirtioBlkConfig>() - 4) as u32,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        let config_bytes = self.config.as_bytes();
        let offset = offset as usize;
        if offset + 4 <= config_bytes.len() {
            u32::from_le_bytes(config_bytes[offset..offset + 4].try_into().unwrap())
        } else if offset < config_bytes.len() {
            let mut bytes = [0u8; 4];
            let len = config_bytes.len() - offset;
            bytes[..len].copy_from_slice(&config_bytes[offset..]);
            u32::from_le_bytes(bytes)
        } else {
            0
        }
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Config space is read-only for virtio-blk.
    }

    fn enable(&mut self, resources: Resources) {
        self.disable();

        let queue_resources = resources.queues.into_iter().next();
        let Some(queue_resources) = queue_resources else {
            return;
        };

        if !queue_resources.params.enable {
            return;
        }

        let queue_event = match PolledWait::new(&self.driver, queue_resources.event) {
            Ok(e) => e,
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to create queue event"
                );
                return;
            }
        };

        let queue = match VirtioQueue::new(
            resources.features,
            queue_resources.params,
            self.memory.clone(),
            queue_resources.notify,
            queue_event,
        ) {
            Ok(q) => q,
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to create virtio queue"
                );
                return;
            }
        };

        let mut task = TaskControl::new(WorkerTask { queue });
        task.insert(
            self.driver_source.simple(),
            "virtio-blk-worker",
            WorkerState {
                disk: self.disk.clone(),
                memory: self.memory.clone(),
                read_only: self.read_only,
                stats: WorkerStats::default(),
            },
        );
        task.start();

        self.worker = Some(Worker { task });
    }

    fn disable(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            self.driver
                .spawn("shutdown-virtio-blk", async move {
                    worker.task.stop().await;
                })
                .detach();
        }
    }
}

/// Process a single virtio-blk request.
async fn process_request(
    disk: &Disk,
    mem: &GuestMemory,
    read_only: bool,
    mut work: VirtioQueueCallbackWork,
) -> IoCompletion {
    match process_request_inner(disk, mem, read_only, &work).await {
        Ok((bytes_written, stat)) => {
            if let Err(err) = write_status_byte(mem, &work, VIRTIO_BLK_S_OK) {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write status byte"
                );
            }
            work.complete(bytes_written + 1); // +1 for status byte
            IoCompletion {
                work,
                bytes_written: bytes_written + 1,
                stat,
            }
        }
        Err(status) => {
            if let Err(err) = write_status_byte(mem, &work, status) {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write error status byte"
                );
            }
            work.complete(1); // just the status byte
            IoCompletion {
                work,
                bytes_written: 1,
                stat: IoStat::Error,
            }
        }
    }
}

/// Inner request processing. Returns Ok((data_bytes_written, stat)) on success,
/// or Err(status_code) on failure.
async fn process_request_inner(
    disk: &Disk,
    mem: &GuestMemory,
    read_only: bool,
    work: &VirtioQueueCallbackWork,
) -> Result<(u32, IoStat), u8> {
    // Read the request header from the first (readable) descriptor.
    let mut header_bytes = [0u8; size_of::<VirtioBlkReqHeader>()];
    let header_len = work
        .read(mem, &mut header_bytes)
        .map_err(|_| VIRTIO_BLK_S_IOERR)?;

    if header_len < size_of::<VirtioBlkReqHeader>() {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let header = VirtioBlkReqHeader::read_from_bytes(&header_bytes).unwrap();
    let request_type = header.request_type;
    let sector = header.sector;
    let sector_size = disk.sector_size() as u64;

    match request_type {
        VIRTIO_BLK_T_IN => {
            let disk_sector = sector * 512 / sector_size;
            let bytes = do_io_per_descriptor(disk, mem, work, disk_sector, true).await?;
            Ok((bytes, IoStat::Read))
        }
        VIRTIO_BLK_T_OUT => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let disk_sector = sector * 512 / sector_size;
            let bytes = do_io_per_descriptor(disk, mem, work, disk_sector, false).await?;
            Ok((bytes, IoStat::Write))
        }
        VIRTIO_BLK_T_FLUSH => {
            disk.sync_cache().await.map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Flush))
        }
        VIRTIO_BLK_T_GET_ID => {
            let id = if let Some(disk_id) = disk.disk_id() {
                let mut id_str = [0u8; VIRTIO_BLK_ID_BYTES];
                let hex: String = disk_id.iter().map(|b| format!("{:02x}", b)).collect();
                let copy_len = hex.len().min(VIRTIO_BLK_ID_BYTES);
                id_str[..copy_len].copy_from_slice(&hex.as_bytes()[..copy_len]);
                id_str
            } else {
                *b"openvmm-virtio-blk\0\0"
            };
            work.write(mem, &id).map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((VIRTIO_BLK_ID_BYTES as u32, IoStat::None))
        }
        VIRTIO_BLK_T_DISCARD => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            // Per spec §5.2.6.1: "The unmap bit MUST be zero for discard commands."
            // Per spec §5.2.6.2: "the device MAY deallocate the specified range."
            // Discard is a hint — no data-content guarantee.
            let mut all_bytes = vec![
                0u8;
                size_of::<VirtioBlkReqHeader>()
                    + size_of::<VirtioBlkDiscardWriteZeroes>()
            ];
            let read_len = work
                .read(mem, &mut all_bytes)
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            if read_len < all_bytes.len() {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let seg = VirtioBlkDiscardWriteZeroes::read_from_bytes(
                &all_bytes[size_of::<VirtioBlkReqHeader>()..],
            )
            .unwrap();
            // Spec §5.2.6.2: "the device MUST set the status byte to
            // VIRTIO_BLK_S_UNSUPP for discard commands if the unmap flag is set."
            if seg.flags & VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP != 0 {
                return Err(VIRTIO_BLK_S_UNSUPP);
            }
            let disk_sector = seg.sector * 512 / sector_size;
            let disk_count = seg.num_sectors as u64 * 512 / sector_size;
            disk.unmap(disk_sector, disk_count, false)
                .await
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Discard))
        }
        _ => Err(VIRTIO_BLK_S_UNSUPP),
    }
}

/// Perform read or write I/O by processing each descriptor individually.
///
/// For reads (`is_read = true`), data descriptors are writable (device writes
/// to guest). For writes, data descriptors are readable (device reads from
/// guest). Each descriptor is a contiguous GPA range that maps cleanly to a
/// [`PagedRange`].
async fn do_io_per_descriptor(
    disk: &Disk,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    start_disk_sector: u64,
    is_read: bool,
) -> Result<u32, u8> {
    let sector_size = disk.sector_size() as u64;
    let mut current_sector = start_disk_sector;
    let mut total_data: u64 = 0;

    // For reads: iterate writable descriptors, skip last byte (status).
    // For writes: iterate readable descriptors, skip header bytes.
    let writable = is_read;
    let total_payload = work.get_payload_length(writable);

    let mut skip_bytes: u64 = if !is_read {
        size_of::<VirtioBlkReqHeader>() as u64
    } else {
        0
    };

    // For reads, the status byte is the last byte of the writable area.
    let data_len = if is_read {
        total_payload.saturating_sub(1)
    } else {
        total_payload - skip_bytes
    };

    let mut remaining_data = data_len;

    for payload in &work.payload {
        if payload.writeable != writable || remaining_data == 0 {
            continue;
        }

        let mut addr = payload.address;
        let mut plen = payload.length as u64;

        // Skip header bytes for write operations.
        if skip_bytes > 0 {
            let s = skip_bytes.min(plen);
            addr += s;
            plen -= s;
            skip_bytes -= s;
        }

        if plen == 0 {
            continue;
        }

        // Don't exceed the data area (exclude status byte for reads).
        let chunk = plen.min(remaining_data);
        remaining_data -= chunk;

        let first_gpn = addr / PAGE_SIZE;
        let last_gpn = (addr + chunk - 1) / PAGE_SIZE;
        let gpns: Vec<u64> = (first_gpn..=last_gpn).collect();
        let offset = (addr % PAGE_SIZE) as usize;
        let range = PagedRange::new(offset, chunk as usize, &gpns).ok_or(VIRTIO_BLK_S_IOERR)?;
        let buffers = RequestBuffers::new(mem, range, is_read);

        if is_read {
            disk.read_vectored(&buffers, current_sector)
                .await
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        } else {
            disk.write_vectored(&buffers, current_sector, false)
                .await
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        }

        current_sector += chunk / sector_size;
        total_data += chunk;
    }

    Ok(if is_read { total_data as u32 } else { 0 })
}

/// Write the status byte to the last writable byte in the descriptor chain.
fn write_status_byte(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    status: u8,
) -> Result<(), virtio::VirtioWriteError> {
    let writable_len = work.get_payload_length(true);
    if writable_len == 0 {
        return Err(virtio::VirtioWriteError::NotAllWritten(1));
    }
    work.write_at_offset(writable_len - 1, mem, &[status])
}
