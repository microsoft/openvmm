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

use crate::connection::ConnKey;
use crate::connection::ConnectionInstanceId;
use crate::connection::RxWork;
use crate::spec::VSOCK_HEADER_SIZE;
use crate::spec::VsockPacket;
use crate::spec::VsockPacketBuf;
use anyhow::Context;
use connection::ConnectionManager;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::future::poll_fn;
use futures::stream::Fuse;
use guestmem::GuestMemory;
use guestmem::LockedRange;
use guestmem::LockedRangeImpl;
use guestmem::ranges::PagedRange;
use inspect::InspectMut;
use pal_async::socket::PolledSocket;
use pal_async::wait::PolledWait;
use smallvec::SmallVec;
use spec::VsockConfig;
use spec::VsockHeader;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::path::PathBuf;
use std::pin::Pin;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use unicycle::FuturesUnordered;
use unix_socket::UnixListener;
use virtio::DeviceTraits;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::VirtioQueuePayload;
use virtio::regions::data_regions;
use virtio::regions::try_build_gpn_list;
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
    #[inspect(skip)]
    base_path: PathBuf,
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
                listener,
            }),
            started_queues: [const { None }; QUEUE_COUNT],
            base_path,
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
                tx_queue: self.started_queues[TX_QUEUE_INDEX].take().unwrap().fuse(),
                event_queue: self.started_queues[EVENT_QUEUE_INDEX].take().unwrap(),
                memory: resources.guest_memory.clone(),
                connections: ConnectionManager::new(self.guest_cid, self.base_path.clone()),
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
            self.started_queues[TX_QUEUE_INDEX] = Some(state.tx_queue.into_inner());
            self.started_queues[EVENT_QUEUE_INDEX] = Some(state.event_queue);

            // Drain any pending IO
            self.worker.task_mut().drain().await;
        }

        // Remove the queue state (drops VirtioQueue).
        self.started_queues[idx as usize]
            .take()
            .map(|queue| queue.queue_state())
    }
}

struct VsockWorkerState {
    connections: ConnectionManager,
    rx_queue: VirtioQueue,
    tx_queue: Fuse<VirtioQueue>,
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

struct VsockWorker {
    work: RxWorkQueue,
    write_ready_work: FuturesUnordered<WriteReadyItem>,
    driver: VmTaskDriver,
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
            if let Some(locked) = lock_payload_data(
                &state.memory,
                &work.payload,
                header.len as u64,
                true,
                false,
                LockedIoSlice::new(),
            )? {
                // Process through the relay.
                state
                    .connections
                    .handle_guest_tx(&self.driver, VsockPacket::new(header, &locked.get().0))
            } else {
                let buf_len: usize = work
                    .payload
                    .iter()
                    .map(|p| if p.writeable { 0 } else { p.length as usize })
                    .sum();
                // No data buffer could be locked; read into a temp buffer and process through the relay.
                let mut temp_buf = vec![0u8; buf_len.min(header.len as usize)];
                work.read_at_offset(VSOCK_HEADER_SIZE as u64, &state.memory, &mut temp_buf)?;
                state.connections.handle_guest_tx(
                    &self.driver,
                    VsockPacket::new(header, &[IoSlice::new(&temp_buf)]),
                )
            }
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

    fn write_packet(
        state: &mut VsockWorkerState,
        work: &mut VirtioQueueCallbackWork,
        packet: &VsockPacketBuf,
    ) -> anyhow::Result<()> {
        tracing::info!(?packet.header, "sending reply");
        let header_bytes = packet.header.as_bytes();
        work.write(&state.memory, header_bytes)
            .context("failed to write vsock header to guest rx")?;

        // The data buffer is present if this is an RW packet and the data could not be read
        // directly into the guest buffer.
        if !packet.data.is_empty() {
            work.write_at_offset(header_bytes.len() as u64, &state.memory, &packet.data)
                .context("failed to write vsock data to guest rx")?;
        }

        work.complete(header_bytes.len() as u32 + packet.header.len);
        Ok(())
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

        let (packet, pending_work) =
            state
                .connections
                .get_rx_packet(&state.memory, &self.driver, work.payload(), rx_work);

        if let Some(packet) = packet {
            let mut work = work.consume();
            if let Err(err) = Self::write_packet(state, &mut work, &packet) {
                tracelimit::error_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to write vsock packet"
                );

                // We can't recover from this. Remove the connection so any future attempst to use
                // it will fail.
                state
                    .connections
                    .remove(&ConnKey::from_rx_packet(&packet.header));
            }
        }

        self.queue_pending_work(pending_work);
    }

    async fn drain(&mut self) {
        // Wait for all pending work to complete. This is used during shutdown to ensure all in-flight
        // packets are processed before the device is stopped and queues are dropped.
        while !self.work.is_empty() || !self.write_ready_work.is_empty() {
            futures::select! {
                _ = self.work.next() => (),
                _ = self.write_ready_work.next() => (),
            }
        }
    }
}

impl AsyncRun<VsockWorkerState> for VsockWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut VsockWorkerState,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            loop {
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
                let mut rx_ready =
                    OptionFuture::from(has_rx_work.then(|| self.work.select_next_some()));

                // This future unfortunately borrows state.rx_queue, which means peeked cannot be
                // used below.
                let mut rx_queue_kick = OptionFuture::from(
                    (!has_rx_work).then(|| poll_fn(|cx| state.rx_queue.poll_kick(cx)).fuse()),
                );

                futures::select! {
                    id = self.write_ready_work.select_next_some() => {
                        let pending = state.connections.handle_write_ready(id);
                        self.queue_pending_work(pending);
                    }
                    r = state.tx_queue.select_next_some() => {
                        match r {
                            Ok(work) => self.handle_tx_work(state, work),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error reading from virtio tx queue"
                            ),
                        }
                    }
                    r = rx_ready => {
                        let work = r.unwrap();
                        self.handle_rx_work(state, work);
                    }
                    _ = rx_queue_kick => {
                        // New buffers are available in the rx queue; try to peek again to trigger
                        // processing.
                    }
                    r = self.listener.accept().fuse() => {
                        match r {
                            Ok((stream, _)) => {
                                tracing::trace!("host unix socket accepted");
                                match state.connections.handle_host_connect(&self.driver, stream) {
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
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error accepting host connections"
                            ),
                        }
                    }
                };
            }
        })
        .await?;
        Ok(())
    }
}

// Use SmallVec since this will nearly always have one item.
struct LockedIoSlice<'a>(SmallVec<[IoSlice<'a>; 4]>);

impl LockedIoSlice<'_> {
    fn new() -> Self {
        Self(SmallVec::new())
    }
}

impl<'a> LockedRange<'a> for LockedIoSlice<'a> {
    fn push_sub_range(&mut self, sub_range: &'a [std::sync::atomic::AtomicU8]) {
        // SAFETY: Treating AtomicU8 as u8 for vectored IO. The lifetime annotations ensure the
        // sub_range lives long enough for the IoSlice.
        let slice =
            unsafe { std::slice::from_raw_parts(sub_range.as_ptr().cast::<u8>(), sub_range.len()) };
        self.0.push(IoSlice::new(slice));
    }
}

struct LockedIoSliceMut<'a>(SmallVec<[IoSliceMut<'a>; 4]>);

impl LockedIoSliceMut<'_> {
    fn new() -> Self {
        Self(SmallVec::new())
    }
}

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
) -> anyhow::Result<Option<LockedRangeImpl<'a, T>>> {
    let regions = data_regions(payload, writable, VSOCK_HEADER_SIZE as u64, data_len);
    let gpn_list = try_build_gpn_list(regions);
    let locked = if let Some((gpns, offset, len)) = &gpn_list {
        if require_exact_len && *len != data_len as usize {
            anyhow::bail!("data length mismatch in vsock tx packet");
        }
        let paged_range =
            PagedRange::new(*offset, *len, gpns).expect("offset and len should be valid");
        Some(mem.lock_range(paged_range, locked_range)?)
    } else {
        None
    };

    Ok(locked)
}
