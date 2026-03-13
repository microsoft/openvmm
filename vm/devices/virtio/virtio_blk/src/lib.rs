// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio block device implementation.

#![forbid(unsafe_code)]

pub mod resolver;
mod spec;

#[cfg(test)]
mod integration_tests;

use crate::spec::*;
use anyhow::Context as _;
use disk_backend::Disk;
use futures::StreamExt;
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
use unicycle::FuturesUnordered;
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
    bounce_ops: Counter,
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
    bounced: bool,
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
        if completion.bounced {
            self.stats.bounce_ops.increment();
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

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
        let queue_resources = resources.queues.into_iter().next();
        let Some(queue_resources) = queue_resources else {
            return Ok(());
        };

        if !queue_resources.params.enable {
            return Ok(());
        }

        let queue_event = PolledWait::new(&self.driver, queue_resources.event)
            .context("failed to create queue event")?;

        let queue = VirtioQueue::new(
            resources.features,
            queue_resources.params,
            self.worker.task().memory.clone(),
            queue_resources.notify,
            queue_event,
        )
        .context("failed to create virtio queue")?;

        self.worker.insert(
            self.driver.clone(),
            "virtio-blk-worker",
            BlkQueueState { queue },
        );
        self.worker.start();
        Ok(())
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
        Ok((bytes_written, stat, bounced)) => {
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
                bounced,
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
                bounced: false,
            }
        }
    }
}

/// Inner request processing. Returns Ok((data_bytes_written, stat, bounced)) on
/// success, or Err(status_code) on failure.
async fn process_request_inner(
    disk: &Disk,
    mem: &GuestMemory,
    read_only: bool,
    work: &VirtioQueueCallbackWork,
) -> Result<(u32, IoStat, bool), u8> {
    // Read the request header from the first (readable) descriptor.
    let mut header = VirtioBlkReqHeader::new_zeroed();
    let header_len = work
        .read(mem, header.as_mut_bytes())
        .map_err(|_| VIRTIO_BLK_S_IOERR)?;

    if header_len < size_of_val(&header) {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let request_type = header.request_type;
    // Shift to convert 512-byte virtio sectors to backend disk sectors.
    // Only meaningful for commands that use sector addressing (IN, OUT, DISCARD).
    let sector_shift = disk.sector_shift() - 9;
    let sector_mask = (1u64 << sector_shift) - 1; // alignment mask for validation

    match request_type {
        VIRTIO_BLK_T_IN => {
            let disk_sector = virtio_to_disk_sector(header.sector, sector_shift, sector_mask)?;
            let (bytes, bounced) = do_io(disk, mem, work, disk_sector, true).await?;
            Ok((bytes, IoStat::Read, bounced))
        }
        VIRTIO_BLK_T_OUT => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let disk_sector = virtio_to_disk_sector(header.sector, sector_shift, sector_mask)?;
            let (bytes, bounced) = do_io(disk, mem, work, disk_sector, false).await?;
            Ok((bytes, IoStat::Write, bounced))
        }
        VIRTIO_BLK_T_FLUSH => {
            disk.sync_cache().await.map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Flush, false))
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
            Ok((VIRTIO_BLK_ID_BYTES as u32, IoStat::None, false))
        }
        VIRTIO_BLK_T_DISCARD => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            // Per spec §5.2.6.1: "The unmap bit MUST be zero for discard commands."
            // Per spec §5.2.6.2: "the device MAY deallocate the specified range."
            // Discard is a hint — no data-content guarantee.

            /// Combined header + discard/write-zeroes segment, used to read
            /// the full request in one `work.read()` call.
            #[repr(C)]
            #[derive(
                zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable, zerocopy::KnownLayout,
            )]
            struct VirtioBlkDiscardReq {
                header: VirtioBlkReqHeader,
                seg: VirtioBlkDiscardWriteZeroes,
            }

            let mut discard_req = VirtioBlkDiscardReq::new_zeroed();
            let read_len = work
                .read(mem, discard_req.as_mut_bytes())
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            if read_len < size_of_val(&discard_req) {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let seg = discard_req.seg;
            // Spec §5.2.6.2: "the device MUST set the status byte to
            // VIRTIO_BLK_S_UNSUPP for discard commands if the unmap flag is set."
            if seg.flags != 0 {
                return Err(VIRTIO_BLK_S_UNSUPP);
            }
            // Discard segment has its own sector/count fields (in 512-byte units).
            let disk_sector = virtio_to_disk_sector(seg.sector, sector_shift, sector_mask)?;
            let num_sectors = seg.num_sectors as u64;
            if num_sectors & sector_mask != 0 {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let disk_count = num_sectors >> sector_shift;
            disk.unmap(disk_sector, disk_count, false)
                .await
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Discard, false))
        }
        _ => Err(VIRTIO_BLK_S_UNSUPP),
    }
}

/// A data-carrying region extracted from a descriptor chain.
///
/// Each entry represents one contiguous GPA range that carries IO data,
/// after header bytes have been skipped and the status byte excluded.
struct DataRegion {
    addr: u64,
    len: u64,
}

/// Convert a 512-byte virtio sector number to a backend disk sector,
/// validating alignment for disks with larger native sectors.
fn virtio_to_disk_sector(
    virtio_sector: u64,
    sector_shift: u32,
    sector_mask: u64,
) -> Result<u64, u8> {
    if virtio_sector & sector_mask != 0 {
        return Err(VIRTIO_BLK_S_IOERR);
    }
    Ok(virtio_sector >> sector_shift)
}

/// Extract the data-carrying regions from a descriptor chain.
///
/// Filters descriptors by direction (`writable`), skips `skip_bytes`
/// (the request header for writes), and limits the total to `data_len`
/// (which excludes the status byte for reads).
fn data_regions(
    payloads: &[virtio::queue::VirtioQueuePayload],
    writable: bool,
    skip_bytes: u64,
    data_len: u64,
) -> Vec<DataRegion> {
    let mut result = Vec::new();
    let mut skip = skip_bytes;
    let mut remaining = data_len;
    for payload in payloads {
        if payload.writeable != writable || remaining == 0 {
            continue;
        }
        let mut addr = payload.address;
        let mut plen = payload.length as u64;
        if skip > 0 {
            let s = skip.min(plen);
            addr += s;
            plen -= s;
            skip -= s;
        }
        if plen == 0 {
            continue;
        }
        let chunk = plen.min(remaining);
        remaining -= chunk;
        result.push(DataRegion { addr, len: chunk });
    }
    result
}

/// Try to build a single `PagedRange` GPN list from the data regions.
///
/// Returns `Some((gpns, offset, len))` if every region boundary falls on
/// a page boundary (or regions are GPA-contiguous), so the whole chain
/// can be expressed as one [`PagedRange`]. Returns `None` if any
/// interior boundary violates the constraint.
fn try_build_gpn_list(regions: &[DataRegion]) -> Option<(Vec<u64>, usize, usize)> {
    const PAGE_SIZE: u64 = guestmem::PAGE_SIZE as u64;

    let mut gpns = Vec::new();
    let mut total_len: u64 = 0;
    let mut first_offset: Option<usize> = None;
    let mut prev_end: Option<u64> = None;

    for region in regions {
        let addr = region.addr;
        let len = region.len;
        if len == 0 {
            continue;
        }

        let first_gpn = addr / PAGE_SIZE;
        let last_gpn = (addr + len - 1) / PAGE_SIZE;

        if let Some(pe) = prev_end {
            if addr == pe {
                // GPA-contiguous with the previous region.
                // The shared page (if any) is already in gpns.
                let last_gpn_in_list = *gpns.last().unwrap();
                if first_gpn == last_gpn_in_list {
                    // Same page — just add any new pages beyond it.
                    for gpn in (first_gpn + 1)..=last_gpn {
                        gpns.push(gpn);
                    }
                } else {
                    // Previous region ended exactly at a page boundary,
                    // so first_gpn is the next page.
                    for gpn in first_gpn..=last_gpn {
                        gpns.push(gpn);
                    }
                }
            } else {
                // Not GPA-contiguous. Both the previous end and this
                // start must be page-aligned to avoid a gap or overlap
                // within a page slot.
                if pe % PAGE_SIZE != 0 || addr % PAGE_SIZE != 0 {
                    return None;
                }
                for gpn in first_gpn..=last_gpn {
                    gpns.push(gpn);
                }
            }
        } else {
            // First region.
            first_offset = Some((addr % PAGE_SIZE) as usize);
            for gpn in first_gpn..=last_gpn {
                gpns.push(gpn);
            }
        }

        prev_end = Some(addr + len);
        total_len += len;
    }

    let offset = first_offset.unwrap_or(0);
    Some((gpns, offset, total_len as usize))
}

/// Owned data needed to construct a `PagedRange`.
///
/// `PagedRange` borrows its GPN slice, so we need to keep the `Vec` alive.
/// This struct holds the owned data alongside the offset and length.
struct OwnedPagedRange {
    gpns: Vec<u64>,
    offset: usize,
    len: usize,
}

/// Copy data between scattered guest regions and a contiguous bounce buffer.
///
/// When `to_bounce` is true (guest→bounce, before write IO), reads from
/// `guest_mem` at scattered `region.addr` addresses and writes
/// contiguously into `bounce_buf`.
///
/// When `to_bounce` is false (bounce→guest, after read IO), reads
/// contiguously from `bounce_buf` and writes to `guest_mem` at
/// scattered `region.addr` addresses.
fn copy_regions(
    bounce_buf: &mut [u8],
    guest_mem: &GuestMemory,
    regions: &[DataRegion],
    to_bounce: bool,
) -> Result<(), u8> {
    let mut linear_offset: usize = 0;
    for region in regions {
        let len = region.len as usize;
        let buf = &mut bounce_buf[linear_offset..linear_offset + len];
        if to_bounce {
            guest_mem
                .read_at(region.addr, buf)
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        } else {
            guest_mem
                .write_at(region.addr, buf)
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        }
        linear_offset += len;
    }
    Ok(())
}

/// Perform read or write I/O for a single request.
///
/// Issues a single `disk.read_vectored` / `disk.write_vectored` call
/// for the entire data payload. If the descriptor chain's memory
/// layout is compatible with [`PagedRange`] (all interior boundaries
/// page-aligned or GPA-contiguous), the IO targets guest memory
/// directly. Otherwise, a bounce buffer is allocated and data is
/// copied through it.
///
/// Returns `Ok((data_bytes_written_to_guest, bounced))` on success.
async fn do_io(
    disk: &Disk,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    start_disk_sector: u64,
    is_read: bool,
) -> Result<(u32, bool), u8> {
    let writable = is_read;
    let total_payload = work.get_payload_length(writable);
    let skip_bytes: u64 = if !is_read {
        size_of::<VirtioBlkReqHeader>() as u64
    } else {
        0
    };
    let data_len = if is_read {
        total_payload.saturating_sub(1)
    } else {
        total_payload.saturating_sub(skip_bytes)
    };

    if data_len == 0 {
        return Ok((0, false));
    }

    // Cap request size to prevent u32 overflow when reporting bytes_written
    // (the caller adds +1 for the status byte). No legitimate IO should be
    // anywhere near this limit.
    if data_len > u32::MAX as u64 - 1 {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    // Validate that the data length is a whole number of backend sectors.
    // The disk backend may panic if given a non-sector-aligned buffer.
    let sector_size = disk.sector_size() as u64;
    if data_len & (sector_size - 1) != 0 {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let regions = data_regions(&work.payload, writable, skip_bytes, data_len);

    let (mut io_mem, io_range, bounced) =
        if let Some((gpns, offset, len)) = try_build_gpn_list(&regions) {
            // Fast path: descriptor chain is PagedRange-compatible.
            (None, OwnedPagedRange { gpns, offset, len }, false)
        } else {
            // Slow path: allocate a bounce buffer.
            // TODO: cap data_len to a reasonable maximum (e.g. seg_max * PAGE_SIZE)
            // to prevent a malicious guest from causing unbounded allocation.
            let data_len_usize = data_len as usize;
            let mut bounce_mem = GuestMemory::allocate(data_len_usize);
            let num_pages = data_len_usize.div_ceil(guestmem::PAGE_SIZE);
            let gpns: Vec<u64> = (0..num_pages as u64).collect();

            if !is_read {
                let buf = bounce_mem.inner_buf_mut().unwrap();
                copy_regions(buf, mem, &regions, true)?;
            }

            (
                Some(bounce_mem),
                OwnedPagedRange {
                    gpns,
                    offset: 0,
                    len: data_len_usize,
                },
                true,
            )
        };

    let effective_mem = io_mem.as_ref().unwrap_or(mem);
    let range =
        PagedRange::new(io_range.offset, io_range.len, &io_range.gpns).ok_or(VIRTIO_BLK_S_IOERR)?;
    let buffers = RequestBuffers::new(effective_mem, range, is_read);

    if is_read {
        disk.read_vectored(&buffers, start_disk_sector)
            .await
            .map_err(|_| VIRTIO_BLK_S_IOERR)?;
    } else {
        disk.write_vectored(&buffers, start_disk_sector, false)
            .await
            .map_err(|_| VIRTIO_BLK_S_IOERR)?;
    }

    if bounced && is_read {
        let buf = io_mem.as_mut().unwrap().inner_buf_mut().unwrap();
        copy_regions(buf, mem, &regions, false)?;
    }

    Ok((if is_read { io_range.len as u32 } else { 0 }, bounced))
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

#[cfg(test)]
mod tests {
    use super::*;
    use virtio::queue::VirtioQueuePayload;

    fn payload(writeable: bool, address: u64, length: u32) -> VirtioQueuePayload {
        VirtioQueuePayload {
            writeable,
            address,
            length,
        }
    }

    // ---- data_regions tests ----

    #[test]
    fn data_regions_read_single_descriptor() {
        // Read: writable descriptors carry data, skip=0, exclude 1 byte for status.
        let payloads = vec![payload(true, 0x1000, 4097)];
        let regions = data_regions(&payloads, true, 0, 4096);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x1000);
        assert_eq!(regions[0].len, 4096);
    }

    #[test]
    fn data_regions_write_skips_header() {
        // Write: readable descriptors carry data, skip header (16 bytes).
        let payloads = vec![
            payload(false, 0x1000, 16),  // header
            payload(false, 0x2000, 512), // data
        ];
        let regions = data_regions(&payloads, false, 16, 512);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x2000);
        assert_eq!(regions[0].len, 512);
    }

    #[test]
    fn data_regions_write_header_spans_descriptors() {
        // Header split across two descriptors.
        let payloads = vec![
            payload(false, 0x1000, 8),   // first 8 bytes of header
            payload(false, 0x2000, 520), // remaining 8 header + 512 data
        ];
        let regions = data_regions(&payloads, false, 16, 512);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x2008); // 0x2000 + 8 skipped
        assert_eq!(regions[0].len, 512);
    }

    #[test]
    fn data_regions_filters_by_direction() {
        // Readable and writable descriptors interleaved.
        let payloads = vec![
            payload(false, 0x1000, 16),  // readable: header
            payload(true, 0x3000, 4097), // writable: data + status
        ];
        // Extract writable regions (read path).
        let regions = data_regions(&payloads, true, 0, 4096);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x3000);
        assert_eq!(regions[0].len, 4096);
    }

    #[test]
    fn data_regions_empty_payload() {
        let payloads: Vec<VirtioQueuePayload> = vec![];
        let regions = data_regions(&payloads, true, 0, 4096);
        assert!(regions.is_empty());
    }

    // ---- try_build_gpn_list tests ----

    #[test]
    fn gpn_list_single_page_aligned_region() {
        let regions = vec![DataRegion {
            addr: 0x1000,
            len: 4096,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1]); // GPN 1 = addr 0x1000
        assert_eq!(offset, 0);
        assert_eq!(len, 4096);
    }

    #[test]
    fn gpn_list_single_region_with_offset() {
        let regions = vec![DataRegion {
            addr: 0x1200,
            len: 512,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1]);
        assert_eq!(offset, 0x200);
        assert_eq!(len, 512);
    }

    #[test]
    fn gpn_list_single_region_spanning_pages() {
        // 8192 bytes starting at page boundary → 2 pages.
        let regions = vec![DataRegion {
            addr: 0x2000,
            len: 8192,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![2, 3]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_two_page_aligned_non_contiguous_regions() {
        // Two regions on different pages, both page-aligned boundaries.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x5000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 5]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_two_gpa_contiguous_regions() {
        // Two regions that are GPA-contiguous (end of first == start of second).
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x2000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 2]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_contiguous_mid_page_boundary() {
        // Two GPA-contiguous regions sharing a page in the middle.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200
            DataRegion {
                addr: 0x2200,
                len: 3584,
            }, // starts at 0x2200, ends at 0x3000
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 2]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_non_contiguous_non_aligned_fails() {
        // Two non-contiguous regions where the boundary isn't page-aligned.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200, not page-aligned
            DataRegion {
                addr: 0x5200,
                len: 512,
            }, // different location, not page-aligned start
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_non_contiguous_first_aligned_second_not() {
        // First ends page-aligned, but second starts mid-page.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            }, // ends at 0x2000 (aligned)
            DataRegion {
                addr: 0x5200,
                len: 512,
            }, // starts at 0x5200 (not aligned)
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_non_contiguous_first_not_aligned_second_aligned() {
        // First ends mid-page, second starts page-aligned.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200 (not aligned)
            DataRegion {
                addr: 0x5000,
                len: 4096,
            }, // starts page-aligned
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_empty_regions() {
        let regions: Vec<DataRegion> = vec![];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert!(gpns.is_empty());
        assert_eq!(offset, 0);
        assert_eq!(len, 0);
    }

    #[test]
    fn gpn_list_three_page_aligned_regions() {
        // Three separate page-aligned regions.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x3000,
                len: 4096,
            },
            DataRegion {
                addr: 0x7000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 3, 7]);
        assert_eq!(offset, 0);
        assert_eq!(len, 12288);
    }

    #[test]
    fn gpn_list_first_region_with_offset_second_page_aligned() {
        // First region starts mid-page but ends at page boundary,
        // second region starts at a different page boundary.
        let regions = vec![
            DataRegion {
                addr: 0x1800,
                len: 2048,
            }, // 0x1800..0x2000
            DataRegion {
                addr: 0x5000,
                len: 4096,
            }, // 0x5000..0x6000
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 5]);
        assert_eq!(offset, 0x800);
        assert_eq!(len, 6144);
    }

    #[test]
    fn gpn_list_validates_paged_range_construction() {
        // Verify that the returned values actually produce a valid PagedRange.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x5000,
                len: 8192,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        let range = PagedRange::new(offset, len, &gpns);
        assert!(range.is_some());
        assert_eq!(range.unwrap().len(), 12288);
    }

    // ---- copy_regions tests ----

    #[test]
    fn copy_regions_roundtrip() {
        // End-to-end: scatter → linear → scatter should preserve data.
        let guest = GuestMemory::allocate(0x10000);
        let mut bounce = [0u8; 2048];

        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 1000,
            },
            DataRegion {
                addr: 0x5000,
                len: 500,
            },
            DataRegion {
                addr: 0x9000,
                len: 548,
            },
        ];

        // Write distinct patterns at each guest region.
        guest.write_at(0x1000, &[0x11; 1000]).unwrap();
        guest.write_at(0x5000, &[0x22; 500]).unwrap();
        guest.write_at(0x9000, &[0x33; 548]).unwrap();

        // Scatter → linear.
        copy_regions(&mut bounce, &guest, &regions, true).unwrap();

        // Clear guest memory to prove the round-trip actually copies back.
        guest.write_at(0x1000, &[0x00; 1000]).unwrap();
        guest.write_at(0x5000, &[0x00; 500]).unwrap();
        guest.write_at(0x9000, &[0x00; 548]).unwrap();

        // Linear → scatter.
        copy_regions(&mut bounce, &guest, &regions, false).unwrap();

        // Verify data restored.
        let mut buf = [0u8; 1000];
        guest.read_at(0x1000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x11));

        let mut buf = [0u8; 500];
        guest.read_at(0x5000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x22));

        let mut buf = [0u8; 548];
        guest.read_at(0x9000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x33));
    }
}
