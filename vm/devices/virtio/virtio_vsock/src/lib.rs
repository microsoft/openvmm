// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio vsock device implementation, per section 5.10 of the virtio specification.

// UNSAFETY: Pointer casts between AtomicU8 and u8 to allow direct read/write into guest memory.
#![expect(unsafe_code)]

mod connections;
pub mod resolver;
mod ring;
mod spec;
mod unix_relay;

#[cfg(test)]
mod integration_tests;

use crate::connections::ConnectionInstanceId;
use crate::connections::ConnectionKey;
use crate::connections::TX_BUF_SIZE;
use crate::spec::Operation;
use crate::spec::VSOCK_HEADER_SIZE;
use crate::spec::VsockFeaturesBank0;
use crate::spec::VsockPacket;
use crate::spec::VsockPacketBuf;
use anyhow::Context;
use connections::ConnectionManager;
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
    /// `guest_cid` is the context ID assigned to the guest.
    ///
    /// `base_path` is the path prefix for Unix socket relay. For a vsock port P, the relay will
    /// attempt to connect to `<base_path>_P`.
    ///
    /// `listener` is an pre-bound Unix listener for accepting host-initiated connections using the
    /// hybrid vsock connect protocol.
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
            worker: TaskControl::new(VsockWorker { driver, listener }),
            started_queues: [const { None }; QUEUE_COUNT],
            base_path,
        })
    }
}

impl VirtioDevice for VirtioVsockDevice {
    fn traits(&self) -> DeviceTraits {
        // Spec 5.10.3.2: The device SHOULD offer the VIRTIO_VSOCK_F_NO_IMPLIED_STREAM feature.
        let features_bank0 = VsockFeaturesBank0::new()
            .with_stream(true)
            .with_no_implied_stream(true);
        DeviceTraits {
            device_id: VirtioDeviceType::VSOCK,
            device_features: VirtioDeviceFeatures::new()
                .with_device_specific_low(features_bank0.into_bits()),
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
        tracelimit::warn_ratelimited!(offset, val, "vsock: unexpected config write");
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: virtio::QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<virtio::queue::QueueState>,
    ) -> anyhow::Result<()> {
        if self
            .started_queues
            .get(idx as usize)
            .ok_or_else(|| anyhow::anyhow!("invalid queue index {idx}"))?
            .is_some()
        {
            anyhow::bail!("virtio queue already started");
        }

        // Spec 5.10.3.2: If no feature bit has been negotiated, the device SHOULD act as if
        // VIRTIO_VSOCK_F_STREAM has been negotiated.
        //
        // If VIRTIO_VSOCK_F_SEQPACKET has been negotiated, but not
        // VIRTIO_VSOCK_F_NO_IMPLIED_STREAM, the device MAY act as if VIRTIO_VSOCK_F_STREAM has also
        // been negotiated.
        let negotiated_features = VsockFeaturesBank0::from_bits(features.bank(0));
        if negotiated_features.no_implied_stream() && !negotiated_features.stream() {
            anyhow::bail!("guest does not support stream sockets");
        }

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create queue event")?;

        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.started_queues[idx as usize] = Some(queue);

        // Start the worker if all queues are started.
        if self.started_queues.iter().all(|q| q.is_some()) {
            let state = VsockWorkerState {
                rx_queue: self.started_queues[RX_QUEUE_INDEX].take().unwrap(),
                tx_queue: self.started_queues[TX_QUEUE_INDEX].take().unwrap().fuse(),
                _event_queue: self.started_queues[EVENT_QUEUE_INDEX].take().unwrap(),
                memory: resources.guest_memory.clone(),
                connections: ConnectionManager::new(self.guest_cid, self.base_path.clone()),
                rx_ready: FuturesUnordered::new(),
                write_ready: FuturesUnordered::new(),
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

            // Transfer the queues back, so we can return the state as each one is stopped
            // individually.
            self.started_queues[RX_QUEUE_INDEX] = Some(state.rx_queue);
            self.started_queues[TX_QUEUE_INDEX] = Some(state.tx_queue.into_inner());
            self.started_queues[EVENT_QUEUE_INDEX] = Some(state._event_queue);
        }

        // Remove the queue state (drops VirtioQueue).
        self.started_queues[idx as usize]
            .take()
            .map(|queue| queue.queue_state())
    }
}

/// Indicates a connection that is read to put data on the rx queue to send to the guest.
/// N.B. When a future returns an item, it's not always guaranteed that a packet is ready to be
///      sent. It could be a spurious wake from a poll, or a pending connection that's still reading
///      its connect request, etc.
pub enum RxReady {
    /// A connection has data or a control packet to send.
    Connection(ConnectionInstanceId),
    /// A pending connection has data.
    PendingConnection(u64),
    /// A RST packet should be sent for a connection that was removed or invalid.
    SendReset(ConnectionKey),
}

/// A pinned future that resolves to an `RxReady` item.
type RxReadyItem = Pin<Box<dyn Future<Output = RxReady> + Send>>;

/// A pinned future that resolves to a `ConnectionInstanceId` for a connection that's ready to write
/// buffered data to the unix socket.
type WriteReadyItem = Pin<Box<dyn Future<Output = ConnectionInstanceId> + Send>>;

/// Represents futures returned from a function that the worker should wait on.
struct PendingFutures {
    rx_ready: Option<RxReadyItem>,
    write_ready: Option<WriteReadyItem>,
}

impl PendingFutures {
    /// A value holding no pending futures.
    const NONE: Self = Self {
        rx_ready: None,
        write_ready: None,
    };

    /// Create a new `PendingFutures` with the given RxReady future, and no WriteReady future.
    fn rx(future: Option<RxReadyItem>) -> Self {
        Self {
            rx_ready: future,
            write_ready: None,
        }
    }

    /// Create a new `PendingFutures` with a future that is immediately ready with the given RxReady
    /// item.
    fn simple_rx(work: RxReady) -> Self {
        Self {
            rx_ready: Some(Box::pin(async move { work })),
            write_ready: None,
        }
    }

    /// Create a new `PendingFutures` with the given WriteReady future and RxReady futures.
    fn new(work: Option<WriteReadyItem>, rx_work: Option<RxReady>) -> Self {
        Self {
            rx_ready: rx_work.map(|w| -> RxReadyItem { Box::pin(async move { w }) }),
            write_ready: work,
        }
    }
}

/// Transient worker state for all three queues.
struct VsockWorkerState {
    connections: ConnectionManager,
    rx_queue: VirtioQueue,
    tx_queue: Fuse<VirtioQueue>,
    // The event queue is not used by this implementation.
    _event_queue: VirtioQueue,
    memory: GuestMemory,
    rx_ready: FuturesUnordered<RxReadyItem>,
    write_ready: FuturesUnordered<WriteReadyItem>,
}

impl VsockWorkerState {
    /// Queue pending futures returned from the connection manager to be processed by the worker run
    /// loop.
    fn queue_pending(&mut self, pending: PendingFutures) {
        if let Some(work) = pending.rx_ready {
            self.rx_ready.push(work);
        }
        if let Some(work) = pending.write_ready {
            self.write_ready.push(work);
        }
    }
}

/// The main worker for the virtio-vsock device.
struct VsockWorker {
    driver: VmTaskDriver,
    listener: PolledSocket<UnixListener>,
}

impl VsockWorker {
    /// Handle a work item from the tx virtqueue (guest -> host).
    fn handle_guest_tx(&mut self, state: &mut VsockWorkerState, work: VirtioQueueCallbackWork) {
        if let Err(err) = self.handle_guest_tx_inner(state, &work) {
            tracelimit::error_ratelimited!(
                error = err.as_ref() as &dyn std::error::Error,
                "error handling vsock tx work"
            );
        }

        state.tx_queue.get_mut().complete(work, 0);
    }

    /// Handle a work item from the TX virtqueue (guest -> host).
    fn handle_guest_tx_inner(
        &mut self,
        state: &mut VsockWorkerState,
        work: &VirtioQueueCallbackWork,
    ) -> anyhow::Result<()> {
        let mut header = VsockHeader::new_zeroed();
        work.read(&state.memory, header.as_mut_bytes())?;

        let rw_len = if header.operation() == Operation::RW {
            // Unaligned field read.
            let len = header.len;

            // The guest should never exceed our available credit, which cannot be larger than the
            // max buffer size. This check prevents a guest from consuming too much host memory if
            // we need to bounce the data through a temporary buffer.
            if len > TX_BUF_SIZE {
                anyhow::bail!("guest attempted to send packet with data length {len}");
            }

            len
        } else {
            // Ignore the length field for other packets (it should always be zero).
            0
        };

        tracing::trace!(?header, "got tx packet from guest");
        let pending = {
            if rw_len == 0 {
                // No payload, so handle the packet immediately.
                state
                    .connections
                    .handle_guest_tx(&self.driver, VsockPacket::new(header, &[]))
            } else if let Some(locked) = lock_payload_data(
                &state.memory,
                &work.payload,
                rw_len as u64,
                true,
                false,
                LockedIoSlice::new(),
            )? {
                // We can read the payload directly from guest memory.
                state
                    .connections
                    .handle_guest_tx(&self.driver, VsockPacket::new(header, &locked.get().0))
            } else {
                // Use a temp bounce buffer if the payload couldn't be locked.
                let mut temp_buf = vec![0u8; rw_len as usize];
                let read_bytes =
                    work.read_at_offset(VSOCK_HEADER_SIZE as u64, &state.memory, &mut temp_buf)?;
                if read_bytes != temp_buf.len() {
                    anyhow::bail!(
                        "expected to read {} bytes of payload, but only read {}",
                        temp_buf.len(),
                        read_bytes
                    );
                }
                state.connections.handle_guest_tx(
                    &self.driver,
                    VsockPacket::new(header, &[IoSlice::new(&temp_buf)]),
                )
            }
        };

        state.queue_pending(pending);
        Ok(())
    }

    /// Helper to write a packet to the RX queue. Returns the number of bytes
    /// written. The caller is responsible for completing the work item.
    fn write_packet(
        state: &VsockWorkerState,
        queue_work: &VirtioQueueCallbackWork,
        packet: &VsockPacketBuf,
    ) -> anyhow::Result<u32> {
        tracing::trace!(?packet.header, "sending reply");
        let header_bytes = packet.header.as_bytes();
        queue_work
            .write(&state.memory, header_bytes)
            .context("failed to write vsock header to guest rx")?;

        // The data buffer is present if only this is an RW packet and the data could not be read
        // directly into the guest buffer.
        if !packet.data.is_empty() {
            queue_work
                .write_at_offset(header_bytes.len() as u64, &state.memory, &packet.data)
                .context("failed to write vsock data to guest rx")?;
        }

        Ok(header_bytes.len() as u32 + packet.header.len)
    }

    /// Try to deliver pending rx packets to the guest via the rx virtqueue.
    fn handle_host_rx(&mut self, state: &mut VsockWorkerState, rx_ready: RxReady) {
        // Due to lifetime issues the PeekedWork cannot be passed into this function so get it
        // back here.
        let peeked_work = state
            .rx_queue
            .try_peek()
            .expect("peek already succeeded before")
            .expect("queue was already checked to have items");

        let (packet, pending) = state.connections.get_rx_packet(
            &state.memory,
            &self.driver,
            peeked_work.payload(),
            rx_ready,
        );

        // If there's a packet to send, write it to the guest.
        if let Some(packet) = packet {
            let queue_work = peeked_work.consume();
            let bytes = match Self::write_packet(state, &queue_work, &packet) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "failed to write vsock packet"
                    );

                    // We can't recover from this. Remove the connection so any future attempts to use
                    // it will fail.
                    state
                        .connections
                        .remove(&ConnectionKey::from_rx_packet(&packet.header));
                    0
                }
            };
            state.rx_queue.complete(queue_work, bytes);
        }

        state.queue_pending(pending);
    }
}

impl AsyncRun<VsockWorkerState> for VsockWorker {
    /// The main worker loop.
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
                    OptionFuture::from(has_rx_work.then(|| state.rx_ready.select_next_some()));

                // This future unfortunately borrows state.rx_queue, which means peeked cannot be
                // used below.
                let mut rx_queue_kick = OptionFuture::from(
                    (!has_rx_work).then(|| poll_fn(|cx| state.rx_queue.poll_kick(cx)).fuse()),
                );

                // Wait for work to do from either host or guest.
                futures::select! {
                    id = state.write_ready.select_next_some() => {
                        let pending = state.connections.handle_write_ready(id);
                        state.queue_pending(pending);
                    }
                    r = state.tx_queue.select_next_some() => {
                        match r {
                            Ok(work) => self.handle_guest_tx(state, work),
                            Err(err) => tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "error reading from virtio tx queue"
                            ),
                        }
                    }
                    r = rx_ready => {
                        let work = r.unwrap();
                        self.handle_host_rx(state, work);
                    }
                    _ = rx_queue_kick => {
                        // New buffers are available in the rx queue; repeat the loop to peek again.
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
                                        state.queue_pending(read_work);
                                        state.queue_pending(timeout_work);
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

// Implementation of LockedRange that collects IoSlice items for use with socket vectored IO.
// Uses SmallVec since this will nearly always have one item.
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

// Same as LockedIoSlice but for mutable buffers.
struct LockedIoSliceMut<'a>(SmallVec<[IoSliceMut<'a>; 4]>);

impl LockedIoSliceMut<'_> {
    fn new() -> Self {
        Self(SmallVec::new())
    }
}

impl<'a> LockedRange<'a> for LockedIoSliceMut<'a> {
    fn push_sub_range(&mut self, sub_range: &'a [std::sync::atomic::AtomicU8]) {
        // SAFETY: Treating AtomicU8 as mut u8 for vectored IO. The lifetime annotations ensure the
        // sub_range lives long enough for the IoSliceMut. Treating the memory as mutable should be
        // safe because AtomicU8 also provides interior mutability.
        let slice = unsafe {
            std::slice::from_raw_parts_mut(sub_range.as_ptr() as *mut u8, sub_range.len())
        };
        self.0.push(IoSliceMut::new(slice));
    }
}

/// Attempts to lock the payload buffers for a virtio request.
///
/// Returns `Ok(Some(...))` if every region boundary falls on a page boundary (or regions are
/// GPA-contiguous), so the whole chain can be expressed as one [`PagedRange`]. Returns `Ok(None)`
/// if any interior boundary violates the constraint.
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
        tracing::trace!("payload data is not representable in a single PagedRange");
        None
    };

    Ok(locked)
}
