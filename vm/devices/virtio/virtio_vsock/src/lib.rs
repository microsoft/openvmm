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

#![forbid(unsafe_code)]

mod protocol;
pub mod relay;
pub mod resolver;

use crate::protocol::VsockPacket;
use crate::relay::RxWork;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use protocol::VIRTIO_DEVICE_TYPE_VSOCK;
use protocol::VsockConfig;
use protocol::VsockHeader;
use relay::VsockRelay;
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
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
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

    fn read_registers_u32(&self, offset: u16) -> u32 {
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

    fn enable(&mut self, resources: Resources) {
        assert!(resources.queues.len() >= QUEUE_COUNT);
        let mut queues = Vec::with_capacity(QUEUE_COUNT);
        let mut resources_iter = resources.queues.into_iter();
        for _ in 0..QUEUE_COUNT {
            let queue_resources = resources_iter.next().expect("not enough queues provided");
            let event = match PolledWait::new(&self.driver, queue_resources.event) {
                Ok(e) => e,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to create event for vsock queue"
                    );
                    return;
                }
            };

            let queue = match VirtioQueue::new(
                resources.features.clone(),
                queue_resources.params,
                self.memory.clone(),
                queue_resources.notify,
                event,
            ) {
                Ok(q) => q,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to create vsock queue"
                    );
                    return;
                }
            };
            queues.push(queue);
        }

        let relay = match VsockRelay::new(
            self.driver.clone(),
            self.guest_cid,
            self.base_path.clone(),
            self.listener.take(),
        ) {
            Ok(r) => r,
            Err(err) => {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to create vsock relay"
                );
                return;
            }
        };

        let state = VsockWorkerState {
            rx_queue: queues.remove(0),
            tx_queue: queues.remove(0),
            event_queue: queues.remove(0),
            relay,
        };

        self.worker
            .insert(self.driver.clone(), "virtio-vsock-worker", state);
        self.worker.start();
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
    relay: VsockRelay,
}

struct VsockWorker {
    mem: GuestMemory,
    work: FuturesUnordered<Pin<Box<dyn Future<Output = RxWork> + Send>>>,
}

impl VsockWorker {
    /// Handle a work item from the tx virtqueue (guest -> host).
    fn handle_tx_work(&mut self, state: &mut VsockWorkerState, work: VirtioQueueCallbackWork) {
        let hdr_size = size_of::<VsockHeader>();
        let readable_len = work.get_payload_length(false) as usize;

        if readable_len < hdr_size {
            tracing::warn!(readable_len, "vsock tx packet too small for header");
            return;
        }

        // Read the full readable payload (header + data).
        let mut buf = vec![0u8; readable_len];
        if let Err(err) = work.read(&self.mem, &mut buf) {
            tracing::error!(
                error = &err as &dyn std::error::Error,
                "failed to read vsock packet from guest"
            );
            return;
        }

        let hdr = match VsockHeader::read_from_bytes(&buf[..hdr_size]) {
            Ok(h) => h,
            Err(_) => {
                tracing::error!("failed to parse vsock header");
                return;
            }
        };

        let data = &buf[hdr_size..];
        tracing::info!(?hdr, len = data.len(), "vsock tx request");

        // Process through the relay.
        if let Some(work) = state.relay.handle_guest_tx(VsockPacket::new(hdr, data)) {
            tracing::info!("queueing rx work from relay");
            self.work.push(Box::pin(async move { work }));
        }
    }

    /// Try to deliver pending rx packets to the guest via the rx virtqueue.
    async fn handle_rx_work(&mut self, state: &mut VsockWorkerState, rx_work: RxWork) {
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

        if let Some(packet) = state.relay.get_rx_packet(rx_work) {
            tracing::info!(?packet.header, "sending reply");
            let mut work = state
                .rx_queue
                .try_next()
                .expect("error reading stream")
                .expect("must have queue items");
            let header = packet.header.as_bytes();
            work.write(&self.mem, header).expect("TODO");
            work.write_at_offset(header.len() as u64, &self.mem, packet.data)
                .expect("TODO");

            work.complete((header.len() + packet.data.len()) as u32);
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
                    }

                    let event = std::future::poll_fn(|cx| {
                        if let Poll::Ready(item) = state.tx_queue.poll_next_unpin(cx) {
                            let item = item.expect("virtio queue stream never ends");
                            return Poll::Ready(Event::TxWork(item));
                        }

                        // TODO: Only check this if RX space available.
                        if let Poll::Ready(Some(work)) = self.work.poll_next_unpin(cx) {
                            return Poll::Ready(Event::RxWork(work));
                        }

                        Poll::Pending
                    })
                    .await;

                    match event {
                        Event::TxWork(Ok(work)) => {
                            tracing::info!("got tx work from guest");
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
                            tracing::info!("got rx work from relay");
                            self.handle_rx_work(state, work).await;
                        }
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
