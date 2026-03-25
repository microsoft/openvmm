// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio vsock device implementation.
//!
//! Implements section 5.10 of the virtio specification: the socket device
//! provides a guest-to-host communication channel over virtqueues, using the
//! AF_VSOCK address family.
//!
//! The device uses three virtqueues:
//! - Queue 0 (rx): packets from host to guest
//! - Queue 1 (tx): packets from guest to host
//! - Queue 2 (event): asynchronous events (e.g., transport reset)
//!
//! Host-side connectivity is provided through a Unix socket relay, similar to
//! the hybrid vsock model used for Hyper-V sockets. See [`relay`] for details.

#![allow(unsafe_code)]

mod connection;
pub mod resolver;
mod ring;
mod spec;
mod unix_relay;

#[cfg(test)]
mod integration_tests;

use crate::connection::ConnectionInstanceId;
use crate::connection::RxWork;
use crate::spec::VSOCK_HEADER_SIZE;
use crate::spec::VsockPacket;
use anyhow::Context;
use connection::ConnectionManager;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::LockedRange;
use guestmem::LockedRangeImpl;
use guestmem::ranges::PagedRange;
use inspect::InspectMut;
use pal_async::socket::PolledSocket;
use pal_async::wait::PolledWait;
use spec::VsockConfig;
use spec::VsockHeader;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use unicycle::FuturesUnordered;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use virtio::DeviceTraits;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::VirtioQueuePayload;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::VirtioDeviceType;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// The number of virtqueues: rx, tx, event.
const QUEUE_COUNT: usize = 3;
const RX_QUEUE_INDEX: usize = 0;
const TX_QUEUE_INDEX: usize = 1;
const EVENT_QUEUE_INDEX: usize = 2;

/// Virtio vsock device.
#[derive(InspectMut)]
pub struct VirtioVsockDevice {
    guest_cid: u64,
    driver: VmTaskDriver,
    #[inspect(skip)]
    worker: TaskControl<VsockWorker, VsockWorkerState>,
    #[inspect(skip)]
    started_queues: [Option<VirtioQueue>; QUEUE_COUNT],
    stopped_queue_count: usize,
}

impl VirtioVsockDevice {
    /// Create a new virtio-vsock device.
    ///
    /// `guest_cid` is the context ID assigned to the guest. The host always
    /// uses CID 2.
    ///
    /// `base_path` is the path prefix for Unix socket relay. For a vsock port
    /// P, the relay will attempt to connect to `<base_path>_P`.
    ///
    /// `listener` is an optional pre-bound Unix listener for accepting
    /// host-initiated connections using the hybrid vsock connect protocol.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        guest_cid: u64,
        base_path: PathBuf,
        listener: UnixListener,
    ) -> anyhow::Result<Self> {
        let driver = driver_source.simple();
        let listener = PolledSocket::new(&driver, listener)
            .context("failed to create polled socket for vsock relay listener")?;
        Ok(Self {
            guest_cid,
            driver: driver.clone(),
            worker: TaskControl::new(VsockWorker {
                work: FuturesUnordered::new(),
                write_ready_work: FuturesUnordered::new(),
                driver,
                connections: ConnectionManager::new(guest_cid, base_path),
                listener,
            }),
            started_queues: [const { None }; QUEUE_COUNT],
            stopped_queue_count: 0,
        })
    }
}

impl VirtioDevice for VirtioVsockDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::VSOCK,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: QUEUE_COUNT.try_into().unwrap(),
            device_register_length: size_of::<VsockConfig>() as u32,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        // Device config: guest_cid is a 64-bit LE value.
        let config = VsockConfig {
            guest_cid: self.guest_cid.to_le(),
        };
        let bytes = config.as_bytes();
        let offset = offset as usize;
        if offset + 4 <= bytes.len() {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
        } else {
            0
        }
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "vsock: unexpected config write");
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: virtio::QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<virtio::queue::QueueState>,
    ) -> anyhow::Result<()> {
        if idx >= QUEUE_COUNT as u16 {
            anyhow::bail!("invalid virtio queue index");
        }

        if self.started_queues[idx as usize].is_some() {
            anyhow::bail!("virtio queue already started");
        }

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create queue event")?;

        let queue = VirtioQueue::new(
            features.clone(),
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.started_queues[idx as usize] = Some(queue);
        if self.started_queues.iter().all(|q| q.is_some()) {
            let state = VsockWorkerState {
                rx_queue: self.started_queues[RX_QUEUE_INDEX].take().unwrap(),
                tx_queue: self.started_queues[TX_QUEUE_INDEX].take().unwrap(),
                event_queue: self.started_queues[EVENT_QUEUE_INDEX].take().unwrap(),
                memory: resources.guest_memory.clone(),
            };

            self.worker
                .insert(self.driver.clone(), "virtio-vsock-worker", state);
            self.worker.start();
        }

        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<virtio::queue::QueueState> {
        // Stop the worker task (cancels the run loop via until_stopped).
        if self.worker.stop().await {
            let state = self.worker.remove();
            self.started_queues[RX_QUEUE_INDEX] = Some(state.rx_queue);
            self.started_queues[TX_QUEUE_INDEX] = Some(state.tx_queue);
            self.started_queues[EVENT_QUEUE_INDEX] = Some(state.event_queue);

            // Drain in-flight IOs to completion. The FuturesUnordered lives in
            // BlkWorker and survives the stop — its pending disk IO futures are
            // polled here until all descriptors are completed in the used ring.
            // TODO?
            //poll_fn(|cx| self.worker.task_mut().poll_drain(cx)).await;
        }

        // Remove the queue state (drops VirtioQueue).
        self.started_queues[idx as usize]
            .take()
            .map(|queue| queue.queue_state())
    }
}

struct VsockWorkerState {
    rx_queue: VirtioQueue,
    tx_queue: VirtioQueue,
    #[allow(dead_code)] // Required by the spec but not actively polled.
    event_queue: VirtioQueue,
    memory: GuestMemory,
}

type RxWorkItem = Pin<Box<dyn Future<Output = RxWork> + Send>>;
type WriteReadyItem = Pin<Box<dyn Future<Output = ConnectionInstanceId> + Send>>;
type RxWorkQueue = FuturesUnordered<RxWorkItem>;

struct PendingWork {
    rx_work: Option<RxWorkItem>,
    write_ready_work: Option<WriteReadyItem>,
}

impl PendingWork {
    const NONE: Self = Self {
        rx_work: None,
        write_ready_work: None,
    };

    fn rx(work: Option<RxWorkItem>) -> Self {
        Self {
            rx_work: work,
            write_ready_work: None,
        }
    }

    fn simple_rx(work: RxWork) -> Self {
        Self {
            rx_work: Some(Box::pin(async move { work })),
            write_ready_work: None,
        }
    }

    fn new(work: Option<WriteReadyItem>, rx_work: Option<RxWork>) -> Self {
        Self {
            rx_work: rx_work.map(|w| -> RxWorkItem { Box::pin(async move { w }) }),
            write_ready_work: work,
        }
    }
}

// TODO: Only put immutable state in here.
struct VsockWorker {
    work: RxWorkQueue,
    write_ready_work: FuturesUnordered<WriteReadyItem>,
    driver: VmTaskDriver,
    connections: ConnectionManager,
    listener: PolledSocket<UnixListener>,
}

impl VsockWorker {
    fn handle_tx_work(&mut self, state: &mut VsockWorkerState, work: VirtioQueueCallbackWork) {
        if let Err(err) = self.handle_tx_work_inner(state, work) {
            tracelimit::error_ratelimited!(
                error = err.as_ref() as &dyn std::error::Error,
                "error handling vsock tx work"
            );
        }
    }

    /// Handle a work item from the tx virtqueue (guest -> host).
    fn handle_tx_work_inner(
        &mut self,
        state: &mut VsockWorkerState,
        work: VirtioQueueCallbackWork,
    ) -> anyhow::Result<()> {
        let readable_len = work.get_payload_length(false) as usize;

        if readable_len < VSOCK_HEADER_SIZE {
            tracing::warn!(readable_len, "vsock tx packet too small for header");
            anyhow::bail!("vsock tx packet too small for header");
        }

        let mut header = VsockHeader::new_zeroed();
        work.read(
            &state.memory,
            &mut header.as_mut_bytes()[..VSOCK_HEADER_SIZE],
        )?;

        tracing::trace!(?header, "got tx packet from guest");
        let pending_work = {
            // TODO: Avoid allocating.
            let locked = lock_payload_data(
                &state.memory,
                &work.payload,
                header.len as u64,
                true,
                false,
                LockedIoSlice(Vec::new()),
            )?;

            // Process through the relay.
            self.connections
                .handle_guest_tx(&self.driver, VsockPacket::new(header, &locked.get().0))
        };

        self.queue_pending_work(pending_work);
        Ok(())
    }

    fn queue_pending_work(&mut self, pending: PendingWork) {
        if let Some(work) = pending.rx_work {
            self.work.push(work);
        }
        if let Some(work) = pending.write_ready_work {
            self.write_ready_work.push(work);
        }
    }

    /// Try to deliver pending rx packets to the guest via the rx virtqueue.
    fn handle_rx_work(&mut self, state: &mut VsockWorkerState, rx_work: RxWork) {
        // let work = poll_fn(|cx| state.rx_queue.poll_next_unpin(cx))
        //     .await
        //     .expect("vsock rx queue never ends");

        // let work = match work {
        //     Ok(w) => w,
        //     Err(err) => {
        //         tracing::error!(
        //             error = &err as &dyn std::error::Error,
        //             "error reading from vsock rx queue"
        //         );
        //         return;
        //     }
        // };

        let work = state
            .rx_queue
            .try_peek()
            .expect("peek already succeeded before")
            .expect("queue was already checked to have items");

        let (header, pending_work) =
            self.connections
                .get_rx_packet(&state.memory, &self.driver, work.payload(), rx_work);

        if let Some(header) = header {
            let mut work = work.consume();
            tracing::info!(?header, "sending reply");
            let header_bytes = header.as_bytes();
            work.write(&state.memory, header_bytes).expect("TODO");
            work.complete(header_bytes.len() as u32 + header.len);
        }

        self.queue_pending_work(pending_work);

        // while !state.pending_rx.is_empty() {
        //     match state.rx_queue.try_next() {
        //         Ok(Some(mut work)) => {
        //             let (hdr, data) = state.pending_rx.remove(0);
        //             let hdr_bytes = hdr.as_bytes();
        //             let total = hdr_bytes.len() + data.len();

        //             // Write header to the writeable descriptors.
        //             if let Err(err) = work.write(&self.mem, hdr_bytes) {
        //                 tracing::error!(
        //                     error = &err as &dyn std::error::Error,
        //                     "failed to write vsock header to guest rx"
        //                 );
        //                 // Put the packet back.
        //                 state.pending_rx.insert(0, (hdr, data));
        //                 work.complete(0);
        //                 break;
        //             }

        //             // Write data payload after the header.
        //             if !data.is_empty() {
        //                 if let Err(err) =
        //                     work.write_at_offset(hdr_bytes.len() as u64, &self.mem, &data)
        //                 {
        //                     tracing::error!(
        //                         error = &err as &dyn std::error::Error,
        //                         "failed to write vsock data to guest rx"
        //                     );
        //                     work.complete(hdr_bytes.len() as u32);
        //                     continue;
        //                 }
        //             }

        //             work.complete(total as u32);
        //         }
        //         Ok(None) => {
        //             // No buffers available right now; will retry on next kick.
        //             break;
        //         }
        //         Err(err) => {
        //             tracing::error!(
        //                 error = &err as &dyn std::error::Error,
        //                 "vsock rx queue error"
        //             );
        //             break;
        //         }
        //     }
        // }
    }

    fn handle_write_ready(&mut self, id: ConnectionInstanceId) {
        let pending = self.connections.handle_write_ready(id);
        self.queue_pending_work(pending);
    }
}

impl AsyncRun<VsockWorkerState> for VsockWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut VsockWorkerState,
    ) -> Result<(), task_control::Cancelled> {
        while stop
            .until_stopped(async {
                loop {
                    enum Event {
                        TxWork(io::Result<VirtioQueueCallbackWork>),
                        RxWork(RxWork),
                        WriteReady(ConnectionInstanceId),
                        Accept(io::Result<UnixStream>),
                        Retry,
                    }

                    let peeked = match state.rx_queue.try_peek() {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error peeking virtio rx queue"
                            );
                            return false;
                        }
                    };

                    let has_rx_work = peeked.is_some();
                    let event = std::future::poll_fn(|cx| {
                        if let Poll::Ready(Some(item)) = self.write_ready_work.poll_next_unpin(cx) {
                            return Poll::Ready(Event::WriteReady(item));
                        }

                        if let Poll::Ready(item) = state.tx_queue.poll_next_unpin(cx) {
                            let item = item.expect("virtio queue stream never ends");
                            return Poll::Ready(Event::TxWork(item));
                        }

                        if has_rx_work {
                            if let Poll::Ready(Some(work)) = self.work.poll_next_unpin(cx) {
                                return Poll::Ready(Event::RxWork(work));
                            }
                        } else if state.rx_queue.poll_kick(cx) == Poll::Ready(()) {
                            // New buffers are available in the rx queue; try to peek again to trigger processing.
                            return Poll::Ready(Event::Retry);
                        }

                        if let Poll::Ready(result) = self.listener.poll_accept(cx) {
                            return Poll::Ready(Event::Accept(result.map(|(stream, _)| stream)));
                        }

                        Poll::Pending
                    })
                    .await;

                    match event {
                        Event::TxWork(Ok(work)) => {
                            self.handle_tx_work(state, work);
                        }
                        Event::TxWork(Err(err)) => {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error reading from virtio queue"
                            );

                            return false;
                        }
                        Event::RxWork(work) => {
                            self.handle_rx_work(state, work);
                        }
                        Event::WriteReady(id) => {
                            self.handle_write_ready(id);
                        }
                        Event::Accept(Err(err)) => {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error accepting host connections"
                            );

                            return false;
                        }
                        Event::Accept(Ok(stream)) => {
                            tracing::trace!("host unix socket accepted");
                            match self.connections.handle_host_connect(&self.driver, stream) {
                                Err(err) => {
                                    tracing::error!(
                                        error = err.as_ref() as &dyn std::error::Error,
                                        "error handling Unix socket connect"
                                    );
                                }
                                Ok((read_work, timeout_work)) => {
                                    self.queue_pending_work(read_work);
                                    self.queue_pending_work(timeout_work);
                                }
                            }
                        }
                        Event::Retry => (),
                    }
                }
                // // Collect any pending rx packets from the relay.
                // let relay_packets = self.relay.poll_rx_packets();
                // state.pending_rx.extend(relay_packets);

                // // Try to deliver pending rx packets to the guest.
                // self.deliver_rx_packets(state);

                // // Wait for a tx packet from the guest, or an exit signal.
                // let mut tx_next = std::pin::pin!(state.tx_queue.next().fuse());

                // futures::select_biased! {
                //     _ = exit => return false,
                //     work = tx_next => {
                //         match work {
                //             Some(Ok(work)) => {
                //                 self.handle_tx_work(state, work);
                //             }
                //             Some(Err(err)) => {
                //                 tracing::error!(
                //                     error = &err as &dyn std::error::Error,
                //                     "vsock tx queue error"
                //                 );
                //                 return false;
                //             }
                //             None => return false,
                //         }
                //     }
                // }

                // true
            })
            .await?
        {}
        Ok(())
    }
}

/// TODO: Share with virtio_blk
struct DataRegion {
    addr: u64,
    len: u64,
}

/// Extract the data-carrying regions from a descriptor chain.
///
/// Filters descriptors by direction (`writable`), skips `skip_bytes`
/// (the request header for writes), and limits the total to `data_len`
/// (which excludes the status byte for reads).
fn data_regions(
    payloads: &[VirtioQueuePayload],
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

// TODO: Use SmallVec.
struct LockedIoSlice<'a>(Vec<IoSlice<'a>>);

impl<'a> LockedRange<'a> for LockedIoSlice<'a> {
    fn push_sub_range(&mut self, sub_range: &'a [std::sync::atomic::AtomicU8]) {
        // SAFETY: Treating AtomicU8 as u8 for vectored IO. The lifetime annotations ensure the
        // sub_range lives long enough for the IoSlice.
        let slice =
            unsafe { std::slice::from_raw_parts(sub_range.as_ptr().cast::<u8>(), sub_range.len()) };
        self.0.push(IoSlice::new(slice));
    }
}

struct LockedIoSliceMut<'a>(Vec<IoSliceMut<'a>>);

impl<'a> LockedRange<'a> for LockedIoSliceMut<'a> {
    fn push_sub_range(&mut self, sub_range: &'a [std::sync::atomic::AtomicU8]) {
        // SAFETY: Treating AtomicU8 as mut u8 for vectored IO. The lifetime annotations ensure the
        // sub_range lives long enough for the IoSliceMut.
        let slice = unsafe {
            std::slice::from_raw_parts_mut(sub_range.as_ptr() as *mut u8, sub_range.len())
        };
        self.0.push(IoSliceMut::new(slice));
    }
}

fn lock_payload_data<'a, T: LockedRange<'a>>(
    mem: &'a GuestMemory,
    payload: &[VirtioQueuePayload],
    data_len: u64,
    require_exact_len: bool,
    writable: bool,
    locked_range: T,
) -> anyhow::Result<LockedRangeImpl<'a, T>> {
    let regions = data_regions(payload, writable, VSOCK_HEADER_SIZE as u64, data_len);
    let gpn_list = try_build_gpn_list(&regions);
    let locked = if let Some((gpns, offset, len)) = &gpn_list {
        if require_exact_len && *len != data_len as usize {
            anyhow::bail!("data length mismatch in vsock tx packet");
        }
        let paged_range =
            PagedRange::new(*offset, *len, gpns).expect("offset and len should be valid");
        mem.lock_range(paged_range, locked_range)?
    } else {
        todo!("use temp buffer");
    };

    Ok(locked)
}
