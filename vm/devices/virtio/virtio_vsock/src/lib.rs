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

use crate::connection::ConnKey;
use crate::connection::RxWork;
use crate::spec::VsockPacket;
use connection::ConnectionManager;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::LockedRange;
use guestmem::ranges::PagedRange;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use spec::VIRTIO_DEVICE_TYPE_VSOCK;
use spec::VsockConfig;
use spec::VsockHeader;
use std::io::IoSlice;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use unicycle::FuturesUnordered;
use unix_socket::UnixListener;
use virtio::DeviceTraits;
use virtio::PeekedWork;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// The number of virtqueues: rx, tx, event.
const QUEUE_COUNT: usize = 3;

/// Virtio vsock device.
#[derive(InspectMut)]
pub struct VirtioVsockDevice {
    guest_cid: u64,
    #[inspect(skip)]
    base_path: PathBuf,
    #[inspect(skip)]
    listener: Option<UnixListener>,
    memory: GuestMemory,
    driver: VmTaskDriver,
    #[inspect(skip)]
    worker: TaskControl<VsockWorker, VsockWorkerState>,
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
        listener: Option<UnixListener>,
        memory: GuestMemory,
    ) -> Self {
        Self {
            guest_cid,
            base_path,
            listener,
            memory: memory.clone(),
            driver: driver_source.simple(),
            worker: TaskControl::new(VsockWorker {
                mem: memory,
                work: FuturesUnordered::new(),
                write_ready_work: FuturesUnordered::new(),
            }),
        }
    }
}

impl VirtioDevice for VirtioVsockDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VIRTIO_DEVICE_TYPE_VSOCK,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: QUEUE_COUNT.try_into().unwrap(),
            device_register_length: size_of::<VsockConfig>() as u32,
            ..Default::default()
        }
    }

    fn read_registers_u32(&mut self, offset: u16) -> u32 {
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

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "vsock: unexpected config write");
    }

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
        assert!(resources.queues.len() >= QUEUE_COUNT);
        let mut queues = Vec::with_capacity(QUEUE_COUNT);
        let mut resources_iter = resources.queues.into_iter();
        for _ in 0..QUEUE_COUNT {
            let queue_resources = resources_iter.next().expect("not enough queues provided");
            let event = PolledWait::new(&self.driver, queue_resources.event)?;

            let queue = VirtioQueue::new(
                resources.features.clone(),
                queue_resources.params,
                self.memory.clone(),
                queue_resources.notify,
                event,
            )?;
            queues.push(queue);
        }

        let relay = ConnectionManager::new(
            self.driver.clone(),
            self.guest_cid,
            self.base_path.clone(),
            self.listener.take(),
        )?;

        let state = VsockWorkerState {
            rx_queue: queues.remove(0),
            tx_queue: queues.remove(0),
            event_queue: queues.remove(0),
            relay,
        };

        self.worker
            .insert(self.driver.clone(), "virtio-vsock-worker", state);
        self.worker.start();
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        ready!(self.worker.poll_stop(cx));
        if self.worker.has_state() {
            self.worker.remove();
        }
        Poll::Ready(())
    }
}

struct VsockWorkerState {
    rx_queue: VirtioQueue,
    tx_queue: VirtioQueue,
    #[allow(dead_code)] // Required by the spec but not actively polled.
    event_queue: VirtioQueue,
    relay: ConnectionManager,
}

type RxWorkItem = Pin<Box<dyn Future<Output = RxWork> + Send>>;
type WriteReadyItem = Pin<Box<dyn Future<Output = ConnKey> + Send>>;
type RxWorkQueue = FuturesUnordered<RxWorkItem>;

struct PendingWork {
    rx_work: Option<RxWork>,
    write_ready_work: Option<WriteReadyItem>,
}

impl PendingWork {
    const NONE: Self = Self {
        rx_work: None,
        write_ready_work: None,
    };

    fn rx(work: RxWork) -> Self {
        Self {
            rx_work: Some(work),
            write_ready_work: None,
        }
    }

    fn new(work: Option<WriteReadyItem>, rx_work: Option<RxWork>) -> Self {
        Self {
            rx_work,
            write_ready_work: work,
        }
    }
}

struct VsockWorker {
    mem: GuestMemory,
    work: RxWorkQueue,
    write_ready_work: FuturesUnordered<WriteReadyItem>,
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
        let hdr_size = size_of::<VsockHeader>();
        let readable_len = work.get_payload_length(false) as usize;

        if readable_len < hdr_size {
            tracing::warn!(readable_len, "vsock tx packet too small for header");
            anyhow::bail!("vsock tx packet too small for header");
        }

        let mut header = VsockHeader::new_zeroed();
        work.read(&self.mem, &mut header.as_mut_bytes()[..hdr_size])?;

        tracing::info!(?header, "got tx packet from guest");
        let pending_work = {
            // TODO: Avoid allocating.
            let regions = data_regions(&work.payload, false, hdr_size as u64, header.len as u64);
            let gpn_list = try_build_gpn_list(&regions);
            let locked = if let Some((gpns, offset, len)) = &gpn_list {
                if *len != header.len as usize {
                    let key = ConnKey::from_tx_packet(&header);
                    self.work
                        .push(Box::pin(async move { RxWork::SendReset(key) }));
                    anyhow::bail!("data length mismatch in vsock tx packet");
                }
                let paged_range = PagedRange::new(*offset, *len, gpns).unwrap();
                self.mem
                    .lock_range(paged_range, LockedIoSlice(Vec::new()))?
            } else {
                todo!("read into temp buffer");
            };

            // Process through the relay.
            state
                .relay
                .handle_guest_tx(VsockPacket::new(header, &locked.get().0))
        };

        self.queue_pending_work(pending_work);
        Ok(())
    }

    fn queue_pending_work(&mut self, pending: PendingWork) {
        if let Some(work) = pending.rx_work {
            self.work.push(Box::pin(async move { work }));
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

        if let Some(header) = state.relay.get_rx_packet(rx_work) {
            tracing::info!(?header, "sending reply");
            let mut work = state
                .rx_queue
                .try_next()
                .expect("peek already succeeded")
                .expect("queue was already checked to have items");

            let header = header.as_bytes();
            work.write(&self.mem, header).expect("TODO");
            work.complete(header.len() as u32);
        }

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

    fn handle_write_ready(&mut self, state: &mut VsockWorkerState, key: ConnKey) {
        self.queue_pending_work(state.relay.handle_write_ready(key));
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
                        TxWork(std::io::Result<VirtioQueueCallbackWork>),
                        RxWork(RxWork),
                        WriteReady(ConnKey),
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
                    let event = std::future::poll_fn(|cx: &mut Context<'_>| {
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
                        Event::WriteReady(key) => {
                            self.handle_write_ready(state, key);
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
