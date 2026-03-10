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
use pal_async::wait::PolledWait;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
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
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const MAX_IO_DEPTH: usize = 64;

/// The virtio-blk device.
#[derive(InspectMut)]
pub struct VirtioBlkDevice {
    #[inspect(flatten)]
    worker: TaskControl<BlkWorker, BlkQueueState>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    read_only: bool,
    supports_discard: bool,
    config: VirtioBlkConfig,
}

/// Persistent worker state. Survives across enable/disable cycles.
///
/// Holds the disk backend, guest memory, stats counters, and the
/// `FuturesUnordered` that tracks in-flight IOs. The IO futures
/// live here (not in `BlkQueueState`) so they survive when the
/// task is stopped — they're drained in `poll_disable()` before
/// the queue state is removed.
#[derive(Inspect)]
struct BlkWorker {
    disk: Disk,
    #[inspect(skip)]
    memory: GuestMemory,
    read_only: bool,
    #[inspect(flatten)]
    stats: WorkerStats,
    #[inspect(skip)]
    ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoCompletion> + Send>>>,
}

/// Transient queue state, created in `enable()` and removed in `poll_disable()`.
struct BlkQueueState {
    queue: VirtioQueue,
}

#[derive(Inspect, Default)]
struct WorkerStats {
    read_ops: Counter,
    write_ops: Counter,
    flush_ops: Counter,
    discard_ops: Counter,
    errors: Counter,
}

impl InspectTask<BlkQueueState> for BlkWorker {
    fn inspect(&self, req: inspect::Request<'_>, _state: Option<&BlkQueueState>) {
        Inspect::inspect(self, req);
    }
}

/// Result of a completed IO operation, returned from the spawned future
/// back to the main task for stats accumulation and descriptor completion.
struct IoCompletion {
    work: VirtioQueueCallbackWork,
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

impl BlkWorker {
    /// Complete a descriptor and accumulate stats.
    fn finish_io(&mut self, mut completion: IoCompletion) {
        completion.work.complete(completion.bytes_written);
        match completion.stat {
            IoStat::Read => self.stats.read_ops.increment(),
            IoStat::Write => self.stats.write_ops.increment(),
            IoStat::Flush => self.stats.flush_ops.increment(),
            IoStat::Discard => self.stats.discard_ops.increment(),
            IoStat::Error => self.stats.errors.increment(),
            IoStat::None => {}
        }
    }

    /// Poll all in-flight IOs to completion.
    ///
    /// Called during `poll_disable()` after the worker task has been stopped.
    /// The `FuturesUnordered` still holds any IOs that were in flight when
    /// `until_stopped` returned. This drains them, ensuring all descriptor
    /// completions are written to the used ring before the queue is dropped.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match self.ios.poll_next_unpin(cx) {
                Poll::Ready(Some(completion)) => self.finish_io(completion),
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncRun<BlkQueueState> for BlkWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut BlkQueueState,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            loop {
                enum Event {
                    NewWork(Result<VirtioQueueCallbackWork, std::io::Error>),
                    Completed(IoCompletion),
                }

                let event = std::future::poll_fn(|cx| {
                    // Poll for completed IOs first to free up slots.
                    if let Poll::Ready(Some(completion)) = self.ios.poll_next_unpin(cx) {
                        return Poll::Ready(Event::Completed(completion));
                    }
                    // Accept new work if under the depth limit.
                    if self.ios.len() < MAX_IO_DEPTH {
                        if let Poll::Ready(item) = state.queue.poll_next_unpin(cx) {
                            let item = item.expect("virtio queue stream never ends");
                            return Poll::Ready(Event::NewWork(item));
                        }
                    }
                    Poll::Pending
                })
                .await;

                match event {
                    Event::NewWork(Ok(work)) => {
                        let disk = self.disk.clone();
                        let mem = self.memory.clone();
                        let read_only = self.read_only;
                        self.ios.push(Box::pin(async move {
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
                        self.finish_io(completion);
                    }
                }
            }
        })
        .await
    }
}

impl VirtioBlkDevice {
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
            // Maximum bytes in a single segment (VIRTIO_BLK_F_SIZE_MAX). Not
            // specified.
            size_max: 0,
            // Maximum segments per request (VIRTIO_BLK_F_SEG_MAX).
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
            discard_sector_alignment: disk.optimal_unmap_sectors() * (sector_size / 512),
            // Write zeroes fields (VIRTIO_BLK_F_WRITE_ZEROES) — not advertised.
            max_write_zeroes_sectors: 0,
            max_write_zeroes_seg: 0,
            write_zeroes_may_unmap: 0,
            unused1: [0; 3],
            _padding: [0; 4],
        };

        let supports_discard = disk.unmap_behavior() != disk_backend::UnmapBehavior::Ignored;

        Self {
            worker: TaskControl::new(BlkWorker {
                disk,
                memory,
                read_only,
                stats: WorkerStats::default(),
                ios: FuturesUnordered::new(),
            }),
            driver: driver_source.simple(),
            read_only,
            supports_discard,
            config,
        }
    }
}

impl VirtioDevice for VirtioBlkDevice {
    fn traits(&self) -> DeviceTraits {
        let mut features = VIRTIO_BLK_F_SEG_MAX
            | VIRTIO_BLK_F_BLK_SIZE
            | VIRTIO_BLK_F_FLUSH
            | VIRTIO_BLK_F_TOPOLOGY;

        if self.read_only {
            features |= VIRTIO_BLK_F_RO;
        }
        if self.supports_discard {
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
        // The transport reads the device config space as a sequence of u32s.
        // We serialize VirtioBlkConfig to bytes and return the requested
        // 4-byte window. Three cases:
        let config_bytes = self.config.as_bytes();
        let offset = offset as usize;
        if offset + 4 <= config_bytes.len() {
            // Normal case: full u32 within bounds.
            u32::from_le_bytes(config_bytes[offset..offset + 4].try_into().unwrap())
        } else if offset < config_bytes.len() {
            // Partial read at the end of config space: zero-pad the
            // remaining bytes so the transport always gets a full u32.
            let mut bytes = [0u8; 4];
            let len = config_bytes.len() - offset;
            bytes[..len].copy_from_slice(&config_bytes[offset..]);
            u32::from_le_bytes(bytes)
        } else {
            // Completely out of range: return zero.
            0
        }
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Config space is read-only for virtio-blk.
    }

    fn enable(&mut self, resources: Resources) {
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
            self.worker.task().memory.clone(),
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

        self.worker.insert(
            self.driver.clone(),
            "virtio-blk-worker",
            BlkQueueState { queue },
        );
        self.worker.start();
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // Stop the worker task (cancels the run loop via until_stopped).
        ready!(self.worker.poll_stop(cx));
        // Drain in-flight IOs to completion. The FuturesUnordered lives in
        // BlkWorker and survives the stop — its pending disk IO futures are
        // polled here until all descriptors are completed in the used ring.
        ready!(self.worker.task_mut().poll_drain(cx));
        // Remove the queue state (drops VirtioQueue).
        if self.worker.has_state() {
            self.worker.remove();
        }
        Poll::Ready(())
    }
}

/// Process a single virtio-blk request.
///
/// Returns the work item back with completion info so the caller can
/// write the used ring entry. This keeps completion in the main loop,
/// which simplifies future queue API changes.
async fn process_request(
    disk: &Disk,
    mem: &GuestMemory,
    read_only: bool,
    work: VirtioQueueCallbackWork,
) -> IoCompletion {
    match process_request_inner(disk, mem, read_only, &work).await {
        Ok((bytes_written, stat)) => {
            if let Err(err) = write_status_byte(mem, &work, VIRTIO_BLK_S_OK) {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write status byte"
                );
            }
            IoCompletion {
                work,
                bytes_written: bytes_written + 1, // +1 for status byte
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
            IoCompletion {
                work,
                bytes_written: 1, // just the status byte
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
    let mut header = VirtioBlkReqHeader::new_zeroed();
    let header_len = work
        .read(mem, header.as_mut_bytes())
        .map_err(|_| VIRTIO_BLK_S_IOERR)?;

    if header_len < size_of_val(&header) {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let request_type = header.request_type;
    let sector_shift = disk.sector_shift() - 9; // convert from disk sector size to 512-byte units
    let disk_sector = header.sector << sector_shift;

    match request_type {
        VIRTIO_BLK_T_IN => {
            let bytes = do_io_per_descriptor(disk, mem, work, disk_sector, true).await?;
            Ok((bytes, IoStat::Read))
        }
        VIRTIO_BLK_T_OUT => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
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
            let mut all_bytes =
                [0u8; size_of::<VirtioBlkReqHeader>() + size_of::<VirtioBlkDiscardWriteZeroes>()];
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
            if seg.flags != 0 {
                return Err(VIRTIO_BLK_S_UNSUPP);
            }
            let disk_count = (seg.num_sectors as u64) << sector_shift;
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
    let sector_shift = disk.sector_shift();
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
    // For writes, exclude the header bytes we need to skip.
    // Use saturating_sub in both cases to guard against a malicious guest
    // providing a descriptor chain shorter than expected.
    let data_len = if is_read {
        total_payload.saturating_sub(1)
    } else {
        total_payload.saturating_sub(skip_bytes)
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

        const PAGE_SIZE: u64 = guestmem::PAGE_SIZE as u64;

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

        current_sector += chunk >> sector_shift;
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
