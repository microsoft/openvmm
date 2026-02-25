// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(test)]

use super::*;
use crate::Arc;
use crate::GuestMemory;
use crate::Guid;
use crate::InspectMut;
use crate::protocol::Version;
use crate::rndisprot;
use async_trait::async_trait;
use buffers::sub_allocation_size_for_mtu;
use futures::Future;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryFutureExt;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use guestmem::ranges::PagedRanges;
use hvdef::hypercall::HvGuestOsId;
use hvdef::hypercall::HvGuestOsMicrosoft;
use hvdef::hypercall::HvGuestOsMicrosoftIds;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use net_backend::BufferAccess;
use net_backend::DisconnectableEndpoint;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::MultiQueueSupport;
use net_backend::Queue as NetQueue;
use net_backend::QueueConfig;
use net_backend::TxError;
use net_backend::TxOffloadSupport;
use net_backend::null::NullEndpoint;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use test_with_tracing::test;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::ChannelClosed;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OfferResources;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::channel::ChannelHandle;
use vmbus_channel::channel::VmbusDevice;
use vmbus_channel::channel::offer_channel;
use vmbus_channel::gpadl::GpadlId;
use vmbus_channel::gpadl::GpadlMap;
use vmbus_channel::gpadl::GpadlMapView;
use vmbus_channel::gpadl_ring::AlignedGpadlView;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::PAGE_SIZE;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::interrupt::Interrupt;
use vmcore::save_restore::SavedStateBlob;
use vmcore::slim_event::SlimEvent;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vm_task::thread::ThreadDriverBackend;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const VMNIC_CHANNEL_TYPE_GUID: Guid = guid::guid!("f8615163-df3e-46c5-913f-f2d2f965ed0e");

enum ChannelResponse {
    Open(bool),
    Close,
    Gpadl(bool),
    // TeardownGpadl(GpadlId),
    Modify(i32),
}

#[derive(Clone)]
struct MockVmbus {
    pub memory: GuestMemory,
    pub child_info: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
}

impl MockVmbus {
    const MAX_SUPPORTED_CHANNELS: usize = 6;
    // The receive buffer will always need to be at least
    // RX_RESERVED_CONTROL_BUFFERS packets big (eight pages). Then each channel
    // needs four pages for the vmbus send/receive rings, plus at least
    // one packet of receive buffer (rounded up to another page), which is five
    // pages per channel.
    pub const AVAILABLE_GUEST_PAGES: usize = 8 + 5 * Self::MAX_SUPPORTED_CHANNELS;

    pub fn new() -> Self {
        Self {
            memory: GuestMemory::allocate(Self::AVAILABLE_GUEST_PAGES * PAGE_SIZE),
            child_info: Arc::new(futures::lock::Mutex::new(Vec::with_capacity(1))),
        }
    }
}

#[async_trait]
impl ParentBus for MockVmbus {
    async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        self.child_info.lock().await.push(request);
        Ok(OfferResources::new(self.memory.clone(), None))
    }
    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }
    fn use_event(&self) -> bool {
        false
    }
}

struct TestNicEndpointState {
    pub poll_iterations_required: u32,
    // Used to check the last set operation
    pub use_vf: Option<bool>,
    // Used for any queries since use_vf is often reset after check.
    pub last_use_vf: Option<bool>,
    pub vf_state: Option<TestVirtualFunctionState>,
    pub stop_endpoint_counter: usize,
    pub link_status_updater: Option<mesh::Sender<VecDeque<bool>>>,
    pub queues: Vec<mesh::Sender<Vec<u8>>>,
    /// When true (default), `TestNicQueue::tx_avail` returns `(true, N)` so
    /// TX packets are completed synchronously.  When false it returns
    /// `(false, N)`, leaving packets in-flight.
    pub sync_tx: bool,
}

impl TestNicEndpointState {
    pub fn new() -> Arc<parking_lot::Mutex<Self>> {
        Arc::new(parking_lot::Mutex::new(Self {
            poll_iterations_required: 1,
            use_vf: None,
            last_use_vf: None,
            vf_state: None,
            stop_endpoint_counter: 0,
            link_status_updater: None,
            queues: Vec::new(),
            sync_tx: true,
        }))
    }

    pub fn update_link_status(this: &Arc<parking_lot::Mutex<Self>>, link_status: &[bool]) {
        let locked_self = this.lock();
        let link_status_updater = locked_self.link_status_updater.as_ref().unwrap();
        let status_vec = link_status.iter().copied().collect::<VecDeque<bool>>();
        link_status_updater.send(status_vec);
    }
}

struct TestNicEndpointInner {
    pub endpoint_state: Option<Arc<parking_lot::Mutex<TestNicEndpointState>>>,
}

impl TestNicEndpointInner {
    pub fn new(endpoint_state: Option<Arc<parking_lot::Mutex<TestNicEndpointState>>>) -> Self {
        Self { endpoint_state }
    }
}

struct TestNicEndpoint {
    inner: Arc<futures::lock::Mutex<TestNicEndpointInner>>,
    is_ordered: bool,
    tx_offload_support: TxOffloadSupport,
    multiqueue_support: MultiQueueSupport,
    link_status_rx: mesh::Receiver<VecDeque<bool>>,
    pending_link_status_updates: VecDeque<bool>,
}

impl TestNicEndpoint {
    pub fn new(endpoint_state: Option<Arc<parking_lot::Mutex<TestNicEndpointState>>>) -> Self {
        let (link_status_tx, link_status_rx) = mesh::channel();
        if let Some(endpoint_state) = endpoint_state.as_ref() {
            let mut locked_state = endpoint_state.lock();
            locked_state.link_status_updater = Some(link_status_tx);
        }
        let inner = TestNicEndpointInner::new(endpoint_state);
        let tx_offload_support = TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
        };
        let multiqueue_support = MultiQueueSupport {
            max_queues: u16::MAX,
            indirection_table_size: 128,
        };
        Self {
            inner: Arc::new(futures::lock::Mutex::new(inner)),
            is_ordered: true,
            tx_offload_support,
            multiqueue_support,
            link_status_rx,
            pending_link_status_updates: VecDeque::new(),
        }
    }
}

impl InspectMut for TestNicEndpoint {
    fn inspect_mut(&mut self, _req: inspect::Request<'_>) {}
}

#[async_trait]
impl net_backend::Endpoint for TestNicEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "TestNicEndpoint"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        _rss: Option<&net_backend::RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        queues.clear();
        let inner = self.inner.lock().await;
        let sync_tx = inner
            .endpoint_state
            .as_ref()
            .is_none_or(|s| s.lock().sync_tx);
        let senders = config
            .into_iter()
            .map(|config| {
                let (tx, rx) = mesh::channel();
                queues.push(Box::new(TestNicQueue::new(config, rx, sync_tx)));
                tx
            })
            .collect::<Vec<_>>();

        if let Some(endpoint_state) = &inner.endpoint_state {
            let mut locked_data = endpoint_state.lock();
            locked_data.queues = senders;
        }
        Ok(())
    }

    async fn stop(&mut self) {
        let inner = self.inner.lock().await;
        if let Some(endpoint_state) = &inner.endpoint_state {
            let mut locked_data = endpoint_state.lock();
            locked_data.stop_endpoint_counter += 1;
        }
    }

    fn is_ordered(&self) -> bool {
        self.is_ordered
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        self.tx_offload_support
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        self.multiqueue_support
    }

    async fn get_data_path_to_guest_vf(&self) -> anyhow::Result<bool> {
        let locked_inner = self.inner.lock().await;
        let endpoint_state = locked_inner.endpoint_state.as_ref().unwrap();
        let locked_data = endpoint_state.lock();
        match locked_data.last_use_vf {
            Some(to_guest) => Ok(to_guest),
            None => Err(anyhow::anyhow!("Last data path state not set")),
        }
    }

    async fn set_data_path_to_guest_vf(&self, use_vf: bool) -> anyhow::Result<()> {
        tracing::info!(use_vf, "set_data_path_to_guest_vf");
        let inner = self.inner.clone();
        let mut iter = {
            let locked_inner = inner.lock().await;
            let endpoint_state = locked_inner.endpoint_state.as_ref().unwrap();
            let mut locked_data = endpoint_state.lock();
            if locked_data
                .vf_state
                .as_ref()
                .is_none_or(|vf_state| vf_state.is_ready())
            {
                locked_data.use_vf = Some(use_vf);
                locked_data.last_use_vf = Some(use_vf);
                locked_data.poll_iterations_required
            } else {
                if use_vf {
                    anyhow::bail!("VF not ready");
                } else {
                    // VF not available, but switching away from using it.
                    return Ok(());
                }
            }
        };
        std::future::poll_fn(move |cx| {
            if iter <= 1 {
                Poll::Ready(Ok(()))
            } else {
                iter -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        if self.pending_link_status_updates.is_empty() {
            self.pending_link_status_updates
                .append(&mut self.link_status_rx.select_next_some().await);
        }
        EndpointAction::LinkStatusNotify(self.pending_link_status_updates.pop_front().unwrap())
    }
}

#[derive(InspectMut)]
struct TestNicQueue {
    #[inspect(skip)]
    pool: Box<dyn BufferAccess>,
    #[inspect(skip)]
    rx_ids: VecDeque<RxId>,
    #[inspect(skip)]
    rx: mesh::Receiver<Vec<u8>>,
    next_rx_packet: Option<Vec<u8>>,
    sync_tx: bool,
}

impl TestNicQueue {
    pub fn new(config: QueueConfig<'_>, rx: mesh::Receiver<Vec<u8>>, sync_tx: bool) -> Self {
        let rx_ids = config.initial_rx.iter().copied().collect();
        Self {
            pool: config.pool,
            rx_ids,
            rx,
            next_rx_packet: None,
            sync_tx,
        }
    }
}

impl NetQueue for TestNicQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.rx_ids.is_empty() {
            return Poll::Pending;
        }
        if self.next_rx_packet.is_none() {
            let recv = std::pin::pin!(self.rx.recv());
            match std::task::ready!(recv.poll(cx)) {
                Ok(packet) => {
                    self.next_rx_packet = Some(packet);
                }
                Err(err) => {
                    tracing::error!(?err, "Error receiving packet");
                    return Poll::Pending;
                }
            }
        }
        Poll::Ready(())
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.rx_ids.extend(done);
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        if packets.is_empty() || self.rx_ids.is_empty() {
            return Ok(0);
        }

        if self.next_rx_packet.is_none() {
            self.next_rx_packet = self.rx.try_recv().ok();
        }

        if let Some(packet) = self.next_rx_packet.take() {
            let len = packet.len();
            assert!(len > 0);
            let rx_id = self.rx_ids.pop_front().unwrap();
            tracing::info!(rx_id = rx_id.0, ?packet, "returning packet on receive path");
            let mut packet = &packet[..];
            let guest_memory = self.pool.guest_memory().clone();
            for seg in self.pool.guest_addresses(rx_id).iter() {
                // N.B. The packet data is written after the implicit header,
                //      which is 256 bytes long. The header can be written with
                //      self.pool.write_header(...) if desired.
                let write_len = packet.len().min(seg.len as usize);
                tracing::info!(seg.gpa, write_len, "writing packet to guest memory");
                guest_memory
                    .write_at(seg.gpa, &packet[..write_len])
                    .unwrap();
                packet = &packet[write_len..];
                if packet.is_empty() {
                    break;
                }
            }
            packets[0] = rx_id;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn tx_avail(&mut self, packets: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        Ok((self.sync_tx, packets.len()))
    }

    fn tx_poll(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
        Ok(0)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        None
    }
}

struct TestNicDevice {
    pub driver: DefaultDriver,
    pub mock_vmbus: MockVmbus,
    pub offer_input: OfferInput,
    pub next_avail_guest_page: usize,
    pub next_avail_gpadl_id: u32,
    channel: ChannelHandle<Nic>,
}

impl TestNicDevice {
    pub async fn new(driver: &DefaultDriver) -> Self {
        let mock_vmbus = MockVmbus::new();
        Self::new_with_vmbus(driver, mock_vmbus).await
    }

    pub async fn new_with_vmbus(driver: &DefaultDriver, mock_vmbus: MockVmbus) -> Self {
        let builder = Nic::builder();
        let nic = builder.build(
            &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
            Guid::new_random(),
            Box::new(NullEndpoint::new()),
            [1, 2, 3, 4, 5, 6].into(),
            0,
        );
        Self::new_with_nic_and_vmbus(driver, mock_vmbus, nic).await
    }

    pub async fn new_with_nic(driver: &DefaultDriver, nic: Nic) -> Self {
        let mock_vmbus = MockVmbus::new();
        Self::new_with_nic_and_vmbus(driver, mock_vmbus, nic).await
    }

    pub async fn new_with_nic_and_vmbus(
        driver: &DefaultDriver,
        mock_vmbus: MockVmbus,
        nic: Nic,
    ) -> Self {
        let channel = offer_channel(driver, &mock_vmbus, nic)
            .await
            .expect("successful init");

        let offer_input = mock_vmbus.child_info.lock().await.pop().unwrap();

        Self {
            driver: driver.clone(),
            mock_vmbus,
            offer_input,
            next_avail_guest_page: 0,
            next_avail_gpadl_id: 1,
            channel,
        }
    }

    pub async fn revoke_and_new(self) -> Self {
        let mut subchannels = self.mock_vmbus.child_info.lock().await;
        for idx in 1..subchannels.len() + 1 {
            self.send_to_channel(idx as u32, ChannelRequest::Close, (), |_| {
                ChannelResponse::Close
            })
            .await
            .expect("Close request successful");
        }
        subchannels.clear();
        drop(subchannels);

        self.send_to_channel(0, ChannelRequest::Close, (), |_| ChannelResponse::Close)
            .await
            .expect("Close request successful");

        drop(self.offer_input);
        let nic = mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(333))
            .until_cancelled(self.channel.revoke())
            .await
            .unwrap()
            .unwrap();

        Self::new_with_nic_and_vmbus(&self.driver, self.mock_vmbus, nic).await
    }

    pub fn reserve_guest_pages(&mut self, page_count: usize) -> (GpadlId, Vec<u64>) {
        if page_count > MockVmbus::AVAILABLE_GUEST_PAGES - self.next_avail_guest_page {
            panic!(
                "Not enough guest pages available -- need to increase the count to at least {}",
                self.next_avail_guest_page + page_count
            );
        }

        let page_array: Vec<u64> = (self.next_avail_guest_page
            ..self.next_avail_guest_page + page_count + 1)
            .map(|e| {
                if e == self.next_avail_guest_page {
                    (page_count * PAGE_SIZE) as u64
                } else {
                    e as u64 - 1
                }
            })
            .collect();

        let result = (GpadlId(self.next_avail_gpadl_id), page_array);
        self.next_avail_guest_page += page_count;
        self.next_avail_gpadl_id += 1;
        result
    }

    pub async fn add_guest_pages(&mut self, page_count: usize) -> (GpadlId, Vec<u64>) {
        let (id, page_array) = self.reserve_guest_pages(page_count);
        let gpadl_response = self
            .send_to_channel(
                0,
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id,
                    count: 1,
                    buf: page_array.clone(),
                },
                ChannelResponse::Gpadl,
            )
            .await
            .expect("Gpadl request successful");

        if let ChannelResponse::Gpadl(response) = gpadl_response {
            assert_eq!(response, true);
        } else {
            panic!("Unexpected return value");
        }

        (id, page_array)
    }

    async fn send_to_channel<I: 'static + Send, R: 'static + Send>(
        &self,
        idx: u32,
        req: impl FnOnce(Rpc<I, R>) -> ChannelRequest,
        input: I,
        f: impl 'static + Send + FnOnce(R) -> ChannelResponse,
    ) -> Result<ChannelResponse, RpcError> {
        if idx == 0 {
            self.offer_input.request_send.call(req, input).await.map(f)
        } else {
            let idx = idx as usize - 1;
            let child_info = self.mock_vmbus.child_info.lock().await;
            (*child_info)[idx]
                .request_send
                .call(req, input)
                .await
                .map(f)
        }
    }

    async fn connect_vmbus_channel(&mut self) -> TestNicChannel<'_> {
        let gpadl_map = GpadlMap::new();
        let (ring_gpadl_id, page_array) = self.add_guest_pages(4).await;
        gpadl_map.add(
            ring_gpadl_id,
            MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap(),
        );

        let host_to_guest_event = Arc::new(SlimEvent::new());
        let host_to_guest_interrupt = {
            let event = host_to_guest_event.clone();
            Interrupt::from_fn(move || event.signal())
        };

        let open_request = OpenRequest {
            // Channel open-specific data.
            open_data: OpenData {
                target_vp: 0,
                ring_offset: 2,
                ring_gpadl_id,
                event_flag: 1,
                connection_id: 1,
                user_data: UserDefinedData::new_zeroed(),
            },
            // The interrupt used to signal the guest.
            interrupt: host_to_guest_interrupt,
            use_confidential_ring: false,
            use_confidential_external_memory: false,
        };

        let open_response = self
            .send_to_channel(0, ChannelRequest::Open, open_request, ChannelResponse::Open)
            .await
            .expect("open successful");

        let ChannelResponse::Open(true) = open_response else {
            panic!("Unexpected return value");
        };

        let mem = self.mock_vmbus.memory.clone();
        let guest_to_host_interrupt = self.offer_input.event.clone();
        TestNicChannel::new(
            self,
            &mem,
            gpadl_map,
            ring_gpadl_id,
            host_to_guest_event,
            guest_to_host_interrupt,
        )
    }

    async fn connect_vmbus_subchannel(&mut self, idx: u32) -> TestNicSubchannel {
        let gpadl_map = GpadlMap::new();
        let (ring_gpadl_id, page_array) = self.add_guest_pages(4).await;
        gpadl_map.add(
            ring_gpadl_id,
            MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap(),
        );

        let host_to_guest_event = Arc::new(SlimEvent::new());
        let host_to_guest_interrupt = {
            let event = host_to_guest_event.clone();
            Interrupt::from_fn(move || event.signal())
        };

        let open_request = OpenRequest {
            // Channel open-specific data.
            open_data: OpenData {
                target_vp: idx,
                ring_offset: 2,
                ring_gpadl_id,
                event_flag: 1,
                connection_id: 1,
                user_data: UserDefinedData::new_zeroed(),
            },
            // The interrupt used to signal the guest.
            interrupt: host_to_guest_interrupt,
            use_confidential_ring: false,
            use_confidential_external_memory: false,
        };

        let open_response = self
            .send_to_channel(
                idx,
                ChannelRequest::Open,
                open_request,
                ChannelResponse::Open,
            )
            .await
            .expect("open successful");

        let ChannelResponse::Open(true) = open_response else {
            panic!("Unexpected return value");
        };

        let guest_to_host_interrupt = self.offer_input.event.clone();
        TestNicSubchannel::new(
            &self.mock_vmbus.memory,
            gpadl_map,
            ring_gpadl_id,
            host_to_guest_event,
            guest_to_host_interrupt,
        )
    }

    pub fn start_vmbus_channel(&mut self) {
        self.channel.start();
    }

    pub async fn stop_vmbus_channel(&mut self) {
        mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(333))
            .until_cancelled(self.channel.stop())
            .await
            .unwrap();
    }

    pub async fn retarget_vp(&self, vp: u32) {
        let modify_request = ModifyRequest::TargetVp { target_vp: vp };
        let send_request = self.send_to_channel(
            0,
            ChannelRequest::Modify,
            modify_request,
            ChannelResponse::Modify,
        );
        let modify_response = mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(333))
            .until_cancelled(send_request)
            .await
            .expect("response received")
            .expect("modify successful");

        assert!(matches!(modify_response, ChannelResponse::Modify(0)));
    }

    pub async fn save(&mut self) -> anyhow::Result<Option<SavedStateBlob>> {
        mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(333))
            .until_cancelled(self.channel.save())
            .await
            .unwrap()
    }

    pub async fn restore(
        &mut self,
        buffer: SavedStateBlob,
        gpadl_map: Arc<GpadlMap>,
        ring_gpadl_id: GpadlId,
        next_avail_guest_page: usize,
        next_avail_gpadl_id: u32,
        host_to_guest_interrupt: Interrupt,
    ) -> anyhow::Result<()> {
        // Restore the previous memory settings
        assert_eq!(self.next_avail_gpadl_id, 1);
        self.next_avail_gpadl_id = next_avail_gpadl_id;
        assert_eq!(self.next_avail_guest_page, 0);
        self.next_avail_guest_page = next_avail_guest_page;

        let gpadl_map_view = gpadl_map.view();
        let gpadl_map_contents = (1..next_avail_gpadl_id)
            .filter_map(|i| {
                let gpadl_id = GpadlId(i);
                if let Ok(gpadl_view) = gpadl_map_view.map(gpadl_id) {
                    Some((gpadl_id, (*gpadl_view).clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<(GpadlId, MultiPagedRangeBuf)>>();

        mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(1000))
            .until_cancelled(async {
                let restore = std::pin::pin!(self.channel.restore(buffer));
                let mut restore = restore.fuse();
                loop {
                    futures::select! {
                        result = restore => break result,
                        request = self.offer_input.server_request_recv.select_next_some() => {
                            match request {
                                vmbus_channel::bus::ChannelServerRequest::Restore(rpc) => {
                                    let gpadls = gpadl_map_contents.iter().map(|(gpadl_id, pages)| {
                                        vmbus_channel::bus::RestoredGpadl {
                                            request: GpadlRequest {
                                                id: *gpadl_id,
                                                count: 1,
                                                buf: pages.range_buffer().to_vec(),
                                            },
                                            accepted: true,
                                        }
                                    }).collect::<Vec<vmbus_channel::bus::RestoredGpadl>>();
                                    rpc.handle_sync(|_open| {
                                        Ok(vmbus_channel::bus::RestoreResult {
                                            open_request: Some(OpenRequest {
                                                open_data: OpenData {
                                                    target_vp: 0,
                                                    ring_offset: 2,
                                                    ring_gpadl_id,
                                                    event_flag: 1,
                                                    connection_id: 1,
                                                    user_data: UserDefinedData::new_zeroed(),
                                                },
                                                interrupt: host_to_guest_interrupt.clone(),
                                                use_confidential_external_memory: false,
                                                use_confidential_ring: false,
                                            }),
                                            gpadls,
                                        })
                                    })
                                }
                                vmbus_channel::bus::ChannelServerRequest::Revoke(_) => (),
                            }
                        }
                    }
                }
            })
            .await
            .unwrap()?;

        Ok(())
    }
}

struct TestNicSubchannel {
    queue: Queue<GpadlRingMem>,
    _transaction_id: u64,
    _gpadl_map: Arc<GpadlMap>,
    _channel_id: GpadlId,
    _host_to_guest_event: Arc<SlimEvent>,
    _guest_done: Arc<AtomicBool>,
}

impl TestNicSubchannel {
    pub fn new(
        mem: &GuestMemory,
        gpadl_map: Arc<GpadlMap>,
        channel_id: GpadlId,
        host_to_guest_event: Arc<SlimEvent>,
        guest_to_host_interrupt: Interrupt,
    ) -> Self {
        let guest_done = Arc::new(AtomicBool::new(false));
        let channel = gpadl_test_guest_channel(
            mem,
            &gpadl_map.clone().view(),
            channel_id,
            2,
            host_to_guest_event.clone(),
            guest_to_host_interrupt,
            guest_done.clone(),
        );
        let queue = Queue::new(channel).unwrap();
        Self {
            queue,
            _transaction_id: 1,
            _gpadl_map: gpadl_map,
            _channel_id: channel_id,
            _host_to_guest_event: host_to_guest_event,
            _guest_done: guest_done,
        }
    }
}

struct NvspMessage<T> {
    header: protocol::MessageHeader,
    data: T,
    padding: &'static [u8],
}

impl<T: IntoBytes + Immutable + KnownLayout> NvspMessage<T> {
    fn payload(&self) -> [&[u8]; 3] {
        [self.header.as_bytes(), self.data.as_bytes(), self.padding]
    }
}

struct TestNicChannel<'a> {
    pub mtu: u32,
    nic: &'a mut TestNicDevice,
    queue: Queue<GpadlRingMem>,
    transaction_id: u64,
    gpadl_map: Arc<GpadlMap>,
    recv_buf_id: GpadlId,
    send_buf_id: GpadlId,
    send_offset: usize,
    channel_id: GpadlId,
    host_to_guest_event: Arc<SlimEvent>,
    subchannels: HashMap<u32, TestNicSubchannel>,
    _guest_done: Arc<AtomicBool>,
}

impl<'a> TestNicChannel<'a> {
    pub fn new(
        nic: &'a mut TestNicDevice,
        mem: &GuestMemory,
        gpadl_map: Arc<GpadlMap>,
        channel_id: GpadlId,
        host_to_guest_event: Arc<SlimEvent>,
        guest_to_host_interrupt: Interrupt,
    ) -> Self {
        let guest_done = Arc::new(AtomicBool::new(false));
        let channel = gpadl_test_guest_channel(
            mem,
            &gpadl_map.clone().view(),
            channel_id,
            2,
            host_to_guest_event.clone(),
            guest_to_host_interrupt,
            guest_done.clone(),
        );
        let queue = Queue::new(channel).unwrap();
        Self {
            mtu: DEFAULT_MTU,
            nic,
            queue,
            transaction_id: 1,
            gpadl_map,
            recv_buf_id: GpadlId(0),
            send_buf_id: GpadlId(0),
            send_offset: 0,
            channel_id,
            host_to_guest_event,
            subchannels: HashMap::new(),
            _guest_done: guest_done,
        }
    }

    pub async fn read_with_timeout<F, R>(&mut self, timeout: Duration, f: F) -> Result<R, ()>
    where
        F: FnOnce(&IncomingPacket<'_, GpadlRingMem>) -> R,
    {
        let (mut reader, _) = self.queue.split();
        let packet = mesh::CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(reader.read())
            .await
            .map_err(drop)?
            .unwrap();
        Ok(f(&packet))
    }

    pub async fn read_subchannel_with_timeout<F, R>(
        &mut self,
        idx: u32,
        timeout: Duration,
        f: F,
    ) -> Result<R, ()>
    where
        F: FnOnce(&IncomingPacket<'_, GpadlRingMem>) -> R,
    {
        if idx == 0 {
            return self.read_with_timeout(timeout, f).await;
        }

        let (mut reader, _) = self.subchannels.get_mut(&idx).unwrap().queue.split();
        let packet = mesh::CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(reader.read())
            .await
            .map_err(drop)?
            .unwrap();
        Ok(f(&packet))
    }

    pub async fn read_with<F, R>(&mut self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&IncomingPacket<'_, GpadlRingMem>) -> R,
    {
        self.read_with_timeout(Duration::from_millis(333), f).await
    }

    pub async fn read_subchannel_with<F, R>(&mut self, idx: u32, f: F) -> Result<R, ()>
    where
        F: FnOnce(&IncomingPacket<'_, GpadlRingMem>) -> R,
    {
        self.read_subchannel_with_timeout(idx, Duration::from_millis(333), f)
            .await
    }

    pub fn rndis_message_parser(&self) -> RndisMessageParser {
        RndisMessageParser::new(
            self.nic.mock_vmbus.memory.clone(),
            self.gpadl_map.clone(),
            self.recv_buf_id,
        )
    }

    pub async fn read_rndis_control_message_with_timeout<T>(
        &mut self,
        message_type: u32,
        timeout: Duration,
    ) -> Option<T>
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout,
    {
        let parser = self.rndis_message_parser();
        let mut transaction_id = None;
        let message = self
            .read_with_timeout(timeout, |packet| {
                match packet {
                    IncomingPacket::Data(data) => {
                        let (rndis_header, external_ranges) = parser.parse_control_message(data);
                        // Verify message_type matches caller expectations
                        assert_eq!(rndis_header.message_type, message_type);

                        transaction_id = data.transaction_id();
                        Some(parser.get(&external_ranges))
                    }
                    _ => panic!("Unexpected packet!"),
                }
            })
            .await
            .or_else(|_| Ok::<Option<T>, ()>(None))
            .unwrap();

        if let Some(transaction_id) = transaction_id {
            // Complete message
            let message = NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            };
            self.queue
                .split()
                .1
                .try_write(&OutgoingPacket {
                    transaction_id,
                    packet_type: OutgoingPacketType::Completion,
                    payload: &message.payload(),
                })
                .unwrap();
        }

        message
    }

    pub async fn read_rndis_control_message<T>(&mut self, message_type: u32) -> Option<T>
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout,
    {
        self.read_rndis_control_message_with_timeout(message_type, Duration::from_millis(333))
            .await
    }

    pub async fn read_rndis_packet_complete_message_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Option<protocol::Message1SendRndisPacketComplete> {
        self.read_with_timeout(timeout, |packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE
                );
                let completion_data: protocol::Message1SendRndisPacketComplete =
                    reader.read_plain().unwrap();
                Some(completion_data)
            }
            _ => panic!("Unexpected packet!"),
        })
        .await
        .unwrap_or(None)
    }

    pub async fn read_rndis_packet_complete_message(
        &mut self,
    ) -> Option<protocol::Message1SendRndisPacketComplete> {
        self.read_rndis_packet_complete_message_with_timeout(Duration::from_millis(333))
            .await
    }

    pub async fn write(&mut self, packet: OutgoingPacket<'_, '_>) {
        let (_, mut writer) = self.queue.split();
        writer.write(packet).await.unwrap();
    }

    pub async fn write_subchannel(&mut self, idx: u32, packet: OutgoingPacket<'_, '_>) {
        if idx == 0 {
            return self.write(packet).await;
        }

        let (_, mut writer) = self.subchannels.get_mut(&idx).unwrap().queue.split();
        writer.write(packet).await.unwrap();
    }

    pub async fn send_initialize_message(&mut self) {
        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE_TYPE_INIT,
            },
            data: protocol::MessageInit {
                protocol_version: Version::V5 as u32,
                protocol_version2: Version::V6 as u32,
            },
            padding: &[],
        };
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
        self.read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(header.message_type, protocol::MESSAGE_TYPE_INIT_COMPLETE);
                let completion_data: protocol::MessageInitComplete = reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");
    }

    pub async fn send_ndis_config_message(
        &mut self,
        capabilities: protocol::NdisConfigCapabilities,
    ) {
        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
            },
            data: protocol::Message2SendNdisConfig {
                mtu: self.mtu,
                reserved: 0,
                capabilities,
            },
            padding: &[],
        };
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
        self.read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");
    }

    pub async fn send_ndis_version_message(&mut self) {
        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
            },
            data: protocol::Message1SendNdisVersion {
                ndis_major_version: 6,
                ndis_minor_version: 30,
            },
            padding: &[],
        };
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
        self.read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");
    }

    pub async fn send_receive_buffer_message(&mut self, max_subchannels: usize) {
        // Need room for reserved control channel packets and at least one
        // additional packet per channel.
        let min_buffer_pages = ((RX_RESERVED_CONTROL_BUFFERS as usize + 1 + max_subchannels)
            * sub_allocation_size_for_mtu(DEFAULT_MTU) as usize)
            .div_ceil(PAGE_SIZE);
        let (gpadl_handle, page_array) = self.nic.add_guest_pages(min_buffer_pages).await;
        let recv_range = MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap();
        self.gpadl_map.add(gpadl_handle, recv_range);
        self.recv_buf_id = gpadl_handle;

        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
            },
            data: protocol::Message1SendReceiveBuffer {
                gpadl_handle,
                id: 0,
                reserved: 0,
            },
            padding: &[],
        };
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
        self.read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER_COMPLETE
                );
                let completion_data: protocol::Message1SendReceiveBufferComplete =
                    reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");
    }

    pub async fn send_send_buffer_message(&mut self) {
        let (gpadl_handle, page_array) = self.nic.add_guest_pages(1).await;
        let send_range = MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap();
        self.gpadl_map.add(gpadl_handle, send_range);
        self.send_buf_id = gpadl_handle;

        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
            },
            data: protocol::Message1SendSendBuffer {
                gpadl_handle,
                id: 0,
                reserved: 0,
            },
            padding: &[],
        };
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;

        self.read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER_COMPLETE
                );
                let completion_data: protocol::Message1SendSendBufferComplete =
                    reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");
    }

    pub async fn initialize(
        &mut self,
        max_subchannels: usize,
        capabilities: protocol::NdisConfigCapabilities,
    ) {
        self.send_initialize_message().await;
        self.send_ndis_config_message(capabilities).await;
        self.send_ndis_version_message().await;
        self.send_receive_buffer_message(max_subchannels).await;
        self.send_send_buffer_message().await;
    }

    pub async fn send_rndis_control_message_no_completion<
        T: IntoBytes + Immutable + KnownLayout,
    >(
        &mut self,
        message_type: u32,
        message: T,
        extra: &[u8],
    ) {
        let message_length = size_of::<rndisprot::MessageHeader>() + size_of::<T>() + extra.len();
        let mem = self.nic.mock_vmbus.memory.clone();
        let gpadl_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let mut range = PagedRanges::new(&*gpadl_view);
        range.skip(self.send_offset);
        let mut buf_writer = range.writer(&mem);
        buf_writer
            .write(
                rndisprot::MessageHeader {
                    message_type,
                    message_length: message_length as u32,
                }
                .as_bytes(),
            )
            .unwrap();

        buf_writer.write(message.as_bytes()).unwrap();

        if !extra.is_empty() {
            buf_writer.write(extra).unwrap();
        }

        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            },
            data: protocol::Message1SendRndisPacket {
                channel_type: protocol::CONTROL_CHANNEL_TYPE,
                send_buffer_section_index: 0xffffffff,
                send_buffer_section_size: 0,
            },
            padding: &[],
        };
        let gpadl_map_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let gpa_range = gpadl_map_view
            .first()
            .unwrap()
            .subrange(self.send_offset, message_length);
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::GpaDirect(&[gpa_range]),
            payload: &message.payload(),
        })
        .await;
        self.send_offset += message_length;
        self.transaction_id += 1;
    }

    pub async fn send_rndis_control_message<T: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        message_type: u32,
        message: T,
        extra: &[u8],
    ) {
        self.send_rndis_control_message_no_completion(message_type, message, extra)
            .await;
        self.read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                panic!("Unexpected data packet {}", header.message_type);
            }
        })
        .await
        .expect("completion message");
    }

    pub async fn send_rndis_multiplepacket_no_completion<T: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        messages: Vec<T>,
        extras: Vec<Vec<u8>>,
    ) {
        let mem = self.nic.mock_vmbus.memory.clone();
        let gpadl_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let mut buf_writer = PagedRanges::new(&*gpadl_view).writer(&mem);
        let mut total_message_length = 0;

        for (message, extra) in messages.iter().zip(extras.iter()) {
            let message_length = size_of::<rndisprot::MessageHeader>()
                + size_of::<rndisprot::Packet>()
                + extra.len();
            total_message_length += message_length;

            buf_writer
                .write(
                    rndisprot::MessageHeader {
                        message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
                        message_length: message_length as u32,
                    }
                    .as_bytes(),
                )
                .unwrap();

            buf_writer.write(message.as_bytes()).unwrap();
            buf_writer.write(extra.as_bytes()).unwrap();
        }

        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            },
            data: protocol::Message1SendRndisPacket {
                channel_type: protocol::CONTROL_CHANNEL_TYPE,
                send_buffer_section_index: 0xffffffff,
                send_buffer_section_size: 0,
            },
            padding: &[],
        };
        let gpadl_map_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let gpa_range = gpadl_map_view
            .first()
            .unwrap()
            .subrange(0, total_message_length);
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::GpaDirect(&[gpa_range]),
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
    }

    pub async fn send_rndis_packet_offload(
        &mut self,
        data: &[u8],
        tcp_checksum: bool,
        udp_checksum: bool,
        lso: bool,
    ) {
        let mem = self.nic.mock_vmbus.memory.clone();
        let gpadl_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let mut buf_writer = PagedRanges::new(&*gpadl_view).writer(&mem);

        assert!(lso || tcp_checksum || udp_checksum);
        // Calculate packet info length based on the offloads requested
        let per_packet_info_offset = size_of::<rndisprot::Packet>() as u32;
        let mut per_packet_info_length = size_of::<rndisprot::PerPacketInfo>() as u32;
        if tcp_checksum || udp_checksum {
            per_packet_info_length += size_of::<rndisprot::TxTcpIpChecksumInfo>() as u32;
            assert!(!lso);
        }
        if lso {
            per_packet_info_length += size_of::<rndisprot::TcpLsoInfo>() as u32;
            assert!(!(tcp_checksum || udp_checksum));
        }

        let message_length = size_of::<rndisprot::MessageHeader>()
            + size_of::<rndisprot::Packet>()
            + per_packet_info_length as usize
            + data.len();

        // Write RNDIS packet message header
        buf_writer
            .write(
                rndisprot::MessageHeader {
                    message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
                    message_length: message_length as u32,
                }
                .as_bytes(),
            )
            .unwrap();

        let packet = rndisprot::Packet {
            data_offset: per_packet_info_offset + per_packet_info_length,
            data_length: data.len() as u32,
            oob_data_offset: 0,
            oob_data_length: 0,
            num_oob_data_elements: 0,
            per_packet_info_offset,
            per_packet_info_length,
            vc_handle: 0,
            reserved: 0,
        };

        // Write RNDIS packet
        buf_writer.write(packet.as_bytes()).unwrap();

        // Write per-packet info for checksum offload
        const TCP_HEADER_OFFSET: u16 = 34; // Ethernet (14) + IPv4 (20)
        if tcp_checksum || udp_checksum {
            let checksum_info = rndisprot::TxTcpIpChecksumInfo::new_zeroed()
                .set_is_ipv4(true)
                .set_tcp_checksum(tcp_checksum)
                .set_udp_checksum(udp_checksum)
                .set_ip_header_checksum(true)
                .set_tcp_header_offset(TCP_HEADER_OFFSET);

            buf_writer
                .write(
                    rndisprot::PerPacketInfo {
                        size: size_of::<rndisprot::PerPacketInfo>() as u32
                            + size_of::<rndisprot::TxTcpIpChecksumInfo>() as u32,
                        typ: rndisprot::PPI_TCP_IP_CHECKSUM,
                        per_packet_information_offset: size_of::<rndisprot::PerPacketInfo>() as u32,
                    }
                    .as_bytes(),
                )
                .unwrap();
            buf_writer.write(checksum_info.as_bytes()).unwrap();
        }

        // Write per-packet info for LSO
        if lso {
            const NORMAL_MTU: u32 = 1460; // 1500 MTU - 40 bytes (IP + TCP Headers). 8960 would be jumbo frames.
            let maximum_segment_size = NORMAL_MTU;
            let lso_info = rndisprot::TcpLsoInfo(
                maximum_segment_size | ((TCP_HEADER_OFFSET as u32) << 20), // MSS in low 20 bits, TCP header offset in bits 20-29
            );

            buf_writer
                .write(
                    rndisprot::PerPacketInfo {
                        size: size_of::<rndisprot::PerPacketInfo>() as u32
                            + size_of::<rndisprot::TcpLsoInfo>() as u32,
                        typ: rndisprot::PPI_LSO,
                        per_packet_information_offset: size_of::<rndisprot::PerPacketInfo>() as u32,
                    }
                    .as_bytes(),
                )
                .unwrap();
            buf_writer.write(lso_info.as_bytes()).unwrap();
        }

        // Write the packet data
        buf_writer.write(data).unwrap();

        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
            },
            data: protocol::Message1SendRndisPacket {
                channel_type: protocol::DATA_CHANNEL_TYPE,
                send_buffer_section_index: 0xffffffff,
                send_buffer_section_size: 0,
            },
            padding: &[],
        };

        let gpadl_map_view = self.gpadl_map.clone().view().map(self.send_buf_id).unwrap();
        let gpa_range = gpadl_map_view.first().unwrap().subrange(0, message_length);
        self.write(OutgoingPacket {
            transaction_id: self.transaction_id,
            packet_type: OutgoingPacketType::GpaDirect(&[gpa_range]),
            payload: &message.payload(),
        })
        .await;
        self.transaction_id += 1;
    }

    pub async fn connect_subchannel(&mut self, idx: u32) {
        self.subchannels
            .insert(idx, self.nic.connect_vmbus_subchannel(idx).await);
    }

    pub fn start(&mut self) {
        self.nic.start_vmbus_channel();
    }

    pub async fn stop(&mut self) {
        self.nic.stop_vmbus_channel().await;
    }

    pub async fn retarget_vp(&self, vp: u32) {
        self.nic.retarget_vp(vp).await;
    }

    pub async fn save(&mut self) -> anyhow::Result<Option<SavedStateBlob>> {
        self.nic.save().await
    }

    pub async fn restore(
        self,
        nic: &'_ mut TestNicDevice,
        buffer: SavedStateBlob,
    ) -> anyhow::Result<TestNicChannel<'_>> {
        let mem = self.nic.mock_vmbus.memory.clone();
        let guest_to_host_interrupt = nic.offer_input.event.clone();
        let host_to_guest_interrupt = {
            let event = self.host_to_guest_event.clone();
            Interrupt::from_fn(move || event.signal())
        };

        let gpadl_map = self.gpadl_map.clone();
        let channel_id = self.channel_id;
        let next_avail_guest_page = self.nic.next_avail_guest_page;
        let next_avail_gpadl_id = self.nic.next_avail_gpadl_id;

        nic.restore(
            buffer,
            gpadl_map.clone(),
            channel_id,
            next_avail_guest_page,
            next_avail_gpadl_id,
            host_to_guest_interrupt,
        )
        .await?;

        Ok(TestNicChannel::new(
            nic,
            &mem,
            gpadl_map,
            channel_id,
            self.host_to_guest_event,
            guest_to_host_interrupt,
        ))
    }
}

struct RndisMessageParser {
    mem: GuestMemory,
    buf: GpadlView,
}

impl RndisMessageParser {
    pub fn new(mem: GuestMemory, gpadl_map: Arc<GpadlMap>, buf_id: GpadlId) -> Self {
        Self {
            mem,
            buf: gpadl_map.clone().view().map(buf_id).unwrap(),
        }
    }

    pub fn parse_message(
        &self,
        data: &queue::DataPacket<'_, GpadlRingMem>,
        channel_type: u32,
    ) -> (rndisprot::MessageHeader, MultiPagedRangeBuf) {
        // Check for RNDIS packet
        let mut reader = data.reader();
        let header: protocol::MessageHeader = reader.read_plain().unwrap();
        assert_eq!(
            header.message_type,
            protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET
        );
        let rndis_data: protocol::Message1SendRndisPacket = reader.read_plain().unwrap();
        assert_eq!(rndis_data.channel_type, channel_type);

        // Fetch RNDIS packet from external memory
        let mut external_ranges = MultiPagedRangeBuf::new();
        if let Some(id) = data.transfer_buffer_id() {
            assert_eq!(id, 0);

            data.read_transfer_ranges(self.buf.first().unwrap(), &mut external_ranges)
                .unwrap()
        } else {
            data.read_external_ranges(&mut external_ranges).unwrap()
        };
        let mut direct_reader = PagedRanges::new(external_ranges.iter()).reader(&self.mem);

        let rndis_header: rndisprot::MessageHeader = direct_reader.read_plain().unwrap();
        (rndis_header, external_ranges)
    }

    pub fn parse_data_message(
        &self,
        data: &queue::DataPacket<'_, GpadlRingMem>,
    ) -> (rndisprot::MessageHeader, MultiPagedRangeBuf) {
        self.parse_message(data, protocol::DATA_CHANNEL_TYPE)
    }

    pub fn parse_control_message(
        &self,
        data: &queue::DataPacket<'_, GpadlRingMem>,
    ) -> (rndisprot::MessageHeader, MultiPagedRangeBuf) {
        self.parse_message(data, protocol::CONTROL_CHANNEL_TYPE)
    }

    pub fn get<T>(&self, external_ranges: &MultiPagedRangeBuf) -> T
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout,
    {
        let mut reader = PagedRanges::new(external_ranges.iter()).reader(&self.mem);
        assert!(reader.skip(size_of::<rndisprot::MessageHeader>()).is_ok());
        tracing::info!(
            bytes_read = size_of::<T>(),
            bytes_available = reader.len(),
            "parsing packet content"
        );
        reader.read_plain::<T>().unwrap()
    }

    pub fn get_data_packet_content<T>(&self, external_ranges: &MultiPagedRangeBuf) -> T
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout,
    {
        const RX_HEADER_LEN: usize = 256;
        let mut reader = PagedRanges::new(external_ranges.iter()).reader(&self.mem);
        // Skip RNDIS packet header to get to the data.
        assert!(reader.skip(RX_HEADER_LEN).is_ok());
        reader.read_plain::<T>().unwrap()
    }
}

enum TestVirtualFunctionStateChange {
    Update(Rpc<(), ()>),
}

#[derive(Clone)]
struct TestVirtualFunctionState {
    id: Arc<parking_lot::Mutex<Option<u32>>>,
    send_runtime_update: Arc<parking_lot::Mutex<mesh::Sender<TestVirtualFunctionStateChange>>>,
    is_ready: Arc<parking_lot::Mutex<bool>>,
    is_ready_update: Arc<(parking_lot::Mutex<Option<bool>>, event_listener::Event)>,
    oneshot_ready_callback: Arc<parking_lot::Mutex<Option<mesh::OneshotSender<Rpc<bool, ()>>>>>,
}

impl TestVirtualFunctionState {
    pub fn new(
        id: Option<u32>,
        send_runtime_update: mesh::Sender<TestVirtualFunctionStateChange>,
    ) -> Self {
        Self {
            id: Arc::new(parking_lot::Mutex::new(id)),
            send_runtime_update: Arc::new(parking_lot::Mutex::new(send_runtime_update)),
            is_ready: Arc::new(parking_lot::Mutex::new(false)),
            is_ready_update: Default::default(),
            oneshot_ready_callback: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn id(&self) -> Option<u32> {
        *self.id.lock()
    }

    pub async fn update_id(
        &self,
        new_id: Option<u32>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<()> {
        *self.id.lock() = new_id;
        if new_id.is_none() && self.is_ready() {
            self.set_ready(false).await;
        }
        let send_update = self
            .send_runtime_update
            .lock()
            .call(TestVirtualFunctionStateChange::Update, ())
            .map_err(anyhow::Error::from);

        match timeout {
            Some(timeout) => {
                let mut ctx = mesh::CancelContext::new().with_timeout(timeout);
                ctx.until_cancelled(send_update)
                    .map_err(anyhow::Error::from)
                    .await?
            }
            None => send_update.await,
        }
    }

    pub async fn await_ready(&self, is_ready: bool, timeout: Duration) -> Result<(), ()> {
        let mut ctx = mesh::CancelContext::new().with_timeout(timeout);

        loop {
            let listener = self.is_ready_update.1.listen();
            {
                let mut val = self.is_ready_update.0.lock();
                if *val == Some(is_ready) {
                    val.take();
                    return Ok(());
                }
            }
            ctx.until_cancelled(listener).await.map_err(drop)?;
        }
    }

    pub fn is_ready_unchanged(&self) -> bool {
        self.is_ready_update.0.lock().is_none()
    }

    pub fn is_ready(&self) -> bool {
        *self.is_ready.lock()
    }

    pub async fn set_ready(&self, is_ready: bool) {
        tracing::info!(is_ready, "set_ready");
        *self.is_ready.lock() = is_ready;
        let ready_callback = self.oneshot_ready_callback.lock().take();
        if let Some(ready_callback) = ready_callback {
            ready_callback.call(|x| x, is_ready).await.unwrap();
        }
        *self.is_ready_update.0.lock() = Some(is_ready);
        self.is_ready_update.1.notify(usize::MAX);
    }
}

struct TestVirtualFunction {
    state: TestVirtualFunctionState,
    recv_update: mesh::Receiver<TestVirtualFunctionStateChange>,
}

impl TestVirtualFunction {
    pub fn new(id: Option<u32>) -> Self {
        let (tx, rx) = mesh::channel();
        Self {
            state: TestVirtualFunctionState::new(id, tx),
            recv_update: rx,
        }
    }

    pub fn state(&self) -> TestVirtualFunctionState {
        self.state.clone()
    }
}

#[async_trait]
impl VirtualFunction for TestVirtualFunction {
    async fn id(&self) -> Option<u32> {
        self.state.id()
    }
    async fn guest_ready_for_device(&mut self) {
        if self.state.is_ready() {
            return;
        }
        self.state.set_ready(true).await;
        // Wait a random amount of time before completing the request.
        let mut wait_ms: u64 = 0;
        getrandom::fill(wait_ms.as_mut_bytes()).expect("rng failure");
        wait_ms %= 50;
        tracing::info!(id = self.state.id(), wait_ms, "Readying VF...");
        let mut ctx = mesh::CancelContext::new().with_timeout(Duration::from_millis(wait_ms));
        let _ = ctx.until_cancelled(pending::<()>()).await;
        tracing::info!(id = self.state.id(), "VF ready");
    }
    async fn wait_for_state_change(&mut self) -> Rpc<(), ()> {
        match self.recv_update.select_next_some().await {
            TestVirtualFunctionStateChange::Update(rpc) => rpc,
        }
    }
}

fn make_test_guest_rings(
    mem: &GuestMemory,
    gpadl_map: &GpadlMapView,
    gpadl_id: GpadlId,
    ring_offset: u32,
) -> (IncomingRing<GpadlRingMem>, OutgoingRing<GpadlRingMem>) {
    let gpadl = AlignedGpadlView::new(gpadl_map.map(gpadl_id).unwrap()).unwrap();
    let (out_gpadl, in_gpadl) = match gpadl.split(ring_offset) {
        Ok(gpadls) => gpadls,
        Err(_) => panic!("Failed gpadl.split"),
    };
    (
        IncomingRing::new(GpadlRingMem::new(in_gpadl, mem).unwrap()).unwrap(),
        OutgoingRing::new(GpadlRingMem::new(out_gpadl, mem).unwrap()).unwrap(),
    )
}

pub fn gpadl_test_guest_channel(
    mem: &GuestMemory,
    gpadl_map: &GpadlMapView,
    gpadl_id: GpadlId,
    ring_offset: u32,
    host_to_guest_event: Arc<SlimEvent>,
    guest_to_host_interrupt: Interrupt,
    done: Arc<AtomicBool>,
) -> RawAsyncChannel<GpadlRingMem> {
    let (in_ring, out_ring) = make_test_guest_rings(mem, gpadl_map, gpadl_id, ring_offset);
    RawAsyncChannel {
        in_ring,
        out_ring,
        signal: Box::new(EventWithDone {
            local_event: host_to_guest_event,
            remote_interrupt: guest_to_host_interrupt,
            done,
        }),
    }
}

struct EventWithDone {
    remote_interrupt: Interrupt,
    local_event: Arc<SlimEvent>,
    done: Arc<AtomicBool>,
}

impl SignalVmbusChannel for EventWithDone {
    fn signal_remote(&self) {
        self.remote_interrupt.deliver();
    }

    fn poll_for_signal(&self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelClosed>> {
        if self.done.load(Ordering::Relaxed) {
            return Err(ChannelClosed).into();
        }
        self.local_event.poll_wait(cx).map(Ok)
    }
}

#[async_test]
async fn build_nic(driver: DefaultDriver) {
    let builder = Nic::builder();
    let unique_id = Guid::new_random();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
        unique_id,
        Box::new(NullEndpoint::new()),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let offer_params = nic.offer();
    assert_eq!(offer_params.interface_id, VMNIC_CHANNEL_TYPE_GUID);
    assert_eq!(offer_params.instance_id, unique_id);
}

#[async_test]
async fn connect_nic_vmbus(driver: DefaultDriver) {
    let builder = Nic::builder();
    let unique_id = Guid::new_random();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        unique_id,
        Box::new(NullEndpoint::new()),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mock_vmbus = MockVmbus::new();
    let _channel = offer_channel(&driver, &mock_vmbus, nic)
        .await
        .expect("successful init");
}

#[async_test]
async fn send_initial_handshake(driver: DefaultDriver) {
    let mut nic = TestNicDevice::new(&driver).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel.send_initialize_message().await;
}

#[async_test]
async fn initialize_nic(driver: DefaultDriver) {
    let mut nic = TestNicDevice::new(&driver).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
}

#[async_test]
async fn initialize_rndis(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    // Not expecting an association packet because virtual function is not present
    assert!(
        channel
            .read_with(|_| panic!("No packet expected"))
            .await
            .is_err()
    );

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);
}

#[async_test]
async fn initialize_rndis_no_sendbuffer(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;

    channel.send_initialize_message().await;
    channel
        .send_ndis_config_message(protocol::NdisConfigCapabilities::new())
        .await;
    channel.send_ndis_version_message().await;
    channel.send_receive_buffer_message(1).await;
    // Note: send_send_buffer_message() not called
    // Creating a Gpadl for the Rndis Init Message
    let (gpadl_handle, page_array) = channel.nic.add_guest_pages(1).await;
    let send_range = MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap();
    channel.gpadl_map.add(gpadl_handle, send_range);
    channel.send_buf_id = gpadl_handle;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    // Not expecting an association packet because virtual function is not present
    assert!(
        channel
            .read_with(|_| panic!("No packet expected"))
            .await
            .is_err()
    );

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);
}

#[async_test]
#[should_panic]
async fn initialize_rndis_no_sendbuffer_no_recvbuffer(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;

    channel.send_initialize_message().await;
    channel
        .send_ndis_config_message(protocol::NdisConfigCapabilities::new())
        .await;
    channel.send_ndis_version_message().await;
    // Note: send_receive_buffer_message() not called
    // Note: send_send_buffer_message() not called
    // Creating a Gpadl for the Rndis Init Message
    let (gpadl_handle, page_array) = channel.nic.add_guest_pages(1).await;
    let send_range = MultiPagedRangeBuf::from_range_buffer(1, page_array).unwrap();
    channel.gpadl_map.add(gpadl_handle, send_range);
    channel.send_buf_id = gpadl_handle;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;
}

#[async_test]
async fn initialize_rndis_with_vf(driver: DefaultDriver) {
    test_initialize_rndis_with_vf_common(driver, true).await;
}

#[async_test]
async fn initialize_rndis_with_vf_without_completion(driver: DefaultDriver) {
    test_initialize_rndis_with_vf_common(driver, false).await;
}

async fn test_initialize_rndis_with_vf_common(driver: DefaultDriver, with_vf_completion: bool) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, test_vf_state.id().unwrap());
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    // Device will be made ready after packet is sent because Linux netvsc does not send completion packet.
    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    if with_vf_completion {
        channel
            .write(OutgoingPacket {
                transaction_id,
                packet_type: OutgoingPacketType::Completion,
                payload: &[],
            })
            .await;
    }

    assert!(test_vf_state.is_ready_unchanged());

    // send switch data path message
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);

    // send another switch data path, but require a few async iterations to actually switch the path.
    endpoint_state.lock().poll_iterations_required = 20;
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::SYNTHETIC.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), false);
    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);
}

#[async_test]
async fn initialize_rndis_with_vf_alternate_id(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let get_guest_os_id = Box::new(move || -> HvGuestOsId {
        let id: u64 = HvGuestOsMicrosoft::new()
            .with_os_id(HvGuestOsMicrosoftIds::WINDOWS_NT.0)
            .into();
        HvGuestOsId::from(id)
    });
    let builder = Nic::builder();
    let nic = builder
        .virtual_function(test_vf)
        .get_guest_os_id(get_guest_os_id)
        .build(
            &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
            Guid::new_random(),
            Box::new(endpoint),
            [1, 2, 3, 4, 5, 6].into(),
            99,
        );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, 99);
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    // Device will be made ready after packet is sent because Linux netvsc does
    // not send completion packet.
    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(test_vf_state.is_ready_unchanged());

    // send switch data path message
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);
}

#[async_test]
async fn initialize_rndis_with_vf_multi_open(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    //
    // Revoke and open a new vmbus channel. This happens from a normal
    // guest when transitioning from UEFI to the OS, or when the OS does a
    // soft restart. It will also happen when configuring parameters like
    // MTU, which is negotiated early.
    //

    let mut nic = nic.revoke_and_new().await;
    // VF should not be revoked when the vmbus channel is closed
    assert!(test_vf_state.is_ready_unchanged());
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;

    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, test_vf_state.id().unwrap());
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    // send switch data path message
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);

    // send another switch data path, but require a few async iterations to actually switch the path.
    endpoint_state.lock().poll_iterations_required = 20;
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::SYNTHETIC.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), false);
}

#[async_test]
async fn initialize_rndis_with_prev_vf_switch_data_path(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    // Starting device with the data path already switched.
    endpoint_state.lock().last_use_vf = Some(true);
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    // The data path was already switched before the device started, so not
    // expecting any VF state change.
    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_err()
    );

    // send switch data path message
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::SYNTHETIC.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), false);
}

#[async_test]
async fn rndis_handle_packet_errors(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    // Not expecting an association packet because virtual function is not present
    assert!(
        channel
            .read_with(|_| panic!("No packet expected"))
            .await
            .is_err()
    );

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);

    // Send a packet with an invalid data offset.
    // Expecting the channel to handle WorkerError::RndisMessageTooSmall
    channel
        .send_rndis_control_message_no_completion(
            rndisprot::MESSAGE_TYPE_PACKET_MSG,
            rndisprot::Packet {
                data_offset: 7777,
                data_length: 0,
                oob_data_offset: 0,
                oob_data_length: 0,
                num_oob_data_elements: 0,
                per_packet_info_offset: 0,
                per_packet_info_length: 0,
                vc_handle: 0,
                reserved: 0,
            },
            &[],
        )
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::FAILURE);

    // Verify the channel is still processing packets by sending another (also invalid).
    channel
        .send_rndis_control_message_no_completion(
            rndisprot::MESSAGE_TYPE_PACKET_MSG,
            rndisprot::Packet {
                data_offset: 8888,
                data_length: 0,
                oob_data_offset: 0,
                oob_data_length: 0,
                num_oob_data_elements: 0,
                per_packet_info_offset: 0,
                per_packet_info_length: 0,
                vc_handle: 0,
                reserved: 0,
            },
            &[],
        )
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::FAILURE);
}

#[async_test]
async fn stop_start_with_vf(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let _association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    //
    // Test start/stop after VF is added
    //
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    // VF should remain visible through start/stop
    channel.stop().await;
    assert!(test_vf_state.is_ready_unchanged());
    channel.start();
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Test start/stop with VF and data path switched.
    //
    endpoint_state.lock().poll_iterations_required = 5;
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);
    assert!(test_vf_state.is_ready_unchanged());

    // VF should remain visible through start/stop
    channel.stop().await;
    assert!(test_vf_state.is_ready_unchanged());
    channel.start();
    assert!(test_vf_state.is_ready_unchanged());
    // Data path should not be updated.
    assert!(endpoint_state.lock().use_vf.is_none());
}

#[async_test]
async fn save_restore_with_vf(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    let mock_vmbus = nic.mock_vmbus.clone();
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let _association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    //
    // Save/restore.
    //
    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();

    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let old_test_vf_state = test_vf_state;
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus.clone(), nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await.unwrap();
    channel.start();
    // VF should remain unchanged
    assert!(old_test_vf_state.is_ready_unchanged());
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Test save/restore after completion message is sent for VF_ASSOCIATION.
    //
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();

    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let old_test_vf_state = test_vf_state;
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus.clone(), nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await.unwrap();
    channel.start();
    // VF should remain unchanged
    assert!(old_test_vf_state.is_ready_unchanged());
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Test save/restore after VF is added and data path is switched.
    //
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;
    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    endpoint_state.lock().poll_iterations_required = 5;
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);
    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();

    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus, nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await.unwrap();
    channel.start();
    assert!(test_vf_state.is_ready_unchanged());
    // Data path should be unchanged.
    assert!(endpoint_state.lock().use_vf.is_none());
}

#[async_test]
async fn save_restore_with_vf_multi_open(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    let mock_vmbus = nic.mock_vmbus.clone();
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    //
    // Add VF
    //
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    //
    // Disconnect/reconnect vmbus a couple of times, re-establishing connection on the second.
    //
    let mut nic = nic.revoke_and_new().await;
    // VF should not be revoked when the vmbus channel is closed
    assert!(test_vf_state.is_ready_unchanged());
    test_vf_state.set_ready(false).await;
    nic.start_vmbus_channel();
    let _ = nic.connect_vmbus_channel().await;
    let mut nic = nic.revoke_and_new().await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;

    // No network init has been done on newer channels, so VF should not be present.
    assert!(
        test_vf_state
            .await_ready(false, Duration::from_millis(333))
            .await
            .is_ok()
    );

    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    //
    // Respond to the last VF association message.
    //
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    //
    // Invoke save/restore
    //
    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();

    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let old_test_vf_state = test_vf_state;
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus.clone(), nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await.unwrap();
    channel.start();
    assert!(old_test_vf_state.is_ready_unchanged());
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Switch data path.
    //
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    endpoint_state.lock().poll_iterations_required = 5;
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);

    //
    // Disconnect/reconnect vmbus a couple of times, re-establishing connection on the second.
    //
    let mut nic = nic.revoke_and_new().await;
    nic.start_vmbus_channel();
    let _ = nic.connect_vmbus_channel().await;
    let mut nic = nic.revoke_and_new().await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    //
    // Invoke save/restore.
    //
    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();

    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus, nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await.unwrap();
    channel.start();
    assert!(test_vf_state.is_ready_unchanged());
    assert_eq!(endpoint_state.lock().use_vf.take().unwrap_or(false), false);
}

async fn test_save_restore_with_rss_table(
    driver: DefaultDriver,
    restore_indirection_table_size: u16,
) -> anyhow::Result<()> {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    // Initialize channel
    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    let mock_vmbus = nic.mock_vmbus.clone();
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    // RSS parameters
    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 4],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4 * size_of::<u32>() as u16,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [0, 1, 2, 3],
    };
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: size_of::<RssParams>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_params.as_bytes(),
        )
        .await;
    let rndis_parser = channel.rndis_message_parser();

    // Send MESSAGE_TYPE_SET_CMPLT packet
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                let set_complete: rndisprot::SetComplete = rndis_parser.get(&external_ranges);
                assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");

    // Complete the MESSAGE_TYPE_SET_CMPLT packet
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;

    // Invoke save/restore
    channel.stop().await;
    let restore_state = channel.save().await.unwrap().unwrap();
    let endpoint_state = TestNicEndpointState::new();
    let mut endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    // Change the indirection table size on the endpoint
    endpoint.multiqueue_support.indirection_table_size = restore_indirection_table_size;
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );
    let mut nic = TestNicDevice::new_with_nic_and_vmbus(&driver, mock_vmbus.clone(), nic).await;
    let mut channel = channel.restore(&mut nic, restore_state).await?;
    channel.start();
    Ok(())
}

#[async_test]
async fn save_restore_reduced_rss_table_size(driver: DefaultDriver) {
    // Reduce the RSS table size from 128 to 16.
    let result = test_save_restore_with_rss_table(driver, 16).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.to_string(), "saved state is invalid");
    if let Some(cause) = err.source() {
        assert_eq!(cause.to_string(), "reduced indirection table size");
    }
}

#[async_test]
async fn save_restore_same_rss_table_size(driver: DefaultDriver) {
    // Supply the same RSS table size of 128.
    let result = test_save_restore_with_rss_table(driver, 128).await;
    assert!(result.is_ok());
}

#[async_test]
async fn save_restore_increased_rss_table_size(driver: DefaultDriver) {
    // Increase the RSS table size from 128 to 256.
    let result = test_save_restore_with_rss_table(driver, 256).await;
    assert!(result.is_ok());
}

async fn remove_vf_with_async_messages(
    channel: &mut TestNicChannel<'_>,
    test_vf_state: &TestVirtualFunctionState,
) -> anyhow::Result<()> {
    let eject_vf = async {
        let transaction_id = channel
            .read_with(|packet| match packet {
                IncomingPacket::Data(data) => {
                    let mut reader = data.reader();
                    let header: protocol::MessageHeader = reader.read_plain().unwrap();
                    assert_eq!(
                        header.message_type,
                        protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                    );
                    let switch_data: protocol::Message4SwitchDataPath =
                        reader.read_plain().unwrap();
                    assert_eq!(
                        switch_data.active_data_path,
                        protocol::DataPath::SYNTHETIC.0
                    );
                    data.transaction_id().expect("should request completion")
                }
                _ => panic!("Unexpected packet"),
            })
            .await
            .expect("association packet");

        channel
            .write(OutgoingPacket {
                transaction_id,
                packet_type: OutgoingPacketType::Completion,
                payload: &[],
            })
            .await;

        let transaction_id = channel
            .read_with(|packet| match packet {
                IncomingPacket::Data(data) => {
                    let mut reader = data.reader();
                    let header: protocol::MessageHeader = reader.read_plain().unwrap();
                    assert_eq!(
                        header.message_type,
                        protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                    );
                    let association_data: protocol::Message4SendVfAssociation =
                        reader.read_plain().unwrap();
                    assert_eq!(association_data.vf_allocated, 0);
                    data.transaction_id().expect("should request completion")
                }
                _ => panic!("Unexpected packet"),
            })
            .await
            .expect("association packet");

        channel
            .write(OutgoingPacket {
                transaction_id,
                packet_type: OutgoingPacketType::Completion,
                payload: &[],
            })
            .await;

        // Linux guests will switch data path as part of VF ejection.
        let message = NvspMessage {
            header: protocol::MessageHeader {
                message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
            },
            data: protocol::Message4SwitchDataPath {
                active_data_path: protocol::DataPath::SYNTHETIC.0,
            },
            padding: &[],
        };
        channel
            .write(OutgoingPacket {
                transaction_id: 123,
                packet_type: OutgoingPacketType::InBandWithCompletion,
                payload: &message.payload(),
            })
            .await;
        channel
            .read_with(|packet| match packet {
                IncomingPacket::Completion(_) => (),
                _ => panic!("Unexpected packet"),
            })
            .await
            .expect("completion message");
    };

    let eject_vf = std::pin::pin!(eject_vf);
    let mut fused_eject_vf = eject_vf.fuse();
    // Remove VF
    let update_id = std::pin::pin!(test_vf_state.update_id(None, None));
    let mut fused_update_id = update_id.fuse();
    // futures_concurrency::future::try_join seems promising, but unable to get it to work here
    loop {
        futures::select! {
            _ = fused_eject_vf => {}
            result = fused_update_id => result?,
            complete => break,
        }
    }
    Ok(())
}

#[async_test]
async fn dynamic_vf_support(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let (mut proxy_endpoint, mut proxy_endpoint_control) = DisconnectableEndpoint::new();
    proxy_endpoint_control.connect(Box::new(endpoint)).unwrap();
    proxy_endpoint.wait_for_endpoint_action().await;

    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(proxy_endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    //
    // Add VF, but don't switch the data path
    //
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    //
    // Remove VF ID.
    //
    test_vf_state
        .update_id(None, Some(Duration::from_millis(100)))
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 0);
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    test_vf_state
        .await_ready(false, Duration::ZERO)
        .await
        .unwrap();

    //
    // Add back VF capability and switch data path
    //
    test_vf_state
        .update_id(Some(124), Some(Duration::from_millis(100)))
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, test_vf_state.id().unwrap());
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Remove VF ID. The VF state that tracks whether the VF can be offered
    // to the guest should not change when the VF is removed. Since here
    // the VF was in a `can be offered` state, it should stay that way after
    // removing VF.
    //
    let mut ctx = mesh::CancelContext::new().with_timeout(Duration::from_millis(333));
    ctx.until_cancelled(remove_vf_with_async_messages(&mut channel, &test_vf_state))
        .map_err(anyhow::Error::from)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), false);
    test_vf_state
        .await_ready(false, Duration::ZERO)
        .await
        .unwrap();

    //
    // Disconnect and reconnect endpoint
    //
    let mut stop_endpoint_counter = 0;
    let endpoint = proxy_endpoint_control.disconnect().await.unwrap().unwrap();
    proxy_endpoint_control.connect(endpoint).unwrap();
    assert_eq!(
        endpoint_state.lock().stop_endpoint_counter,
        stop_endpoint_counter + 1
    );
    stop_endpoint_counter += 1;
    assert!(test_vf_state.is_ready_unchanged());

    //
    // Add back guest VF and switch data path
    //
    test_vf_state
        .update_id(Some(125), Some(Duration::from_millis(100)))
        .await
        .unwrap();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, test_vf_state.id().unwrap());
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
        },
        data: protocol::Message4SwitchDataPath {
            active_data_path: protocol::DataPath::VF.0,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    PolledTimer::new(&driver)
        .sleep(Duration::from_millis(333))
        .await;
    assert_eq!(endpoint_state.lock().use_vf.take().unwrap(), true);

    //
    // Disconnect endpoint with guest VF still active
    //
    let endpoint = proxy_endpoint_control.disconnect().await.unwrap().unwrap();
    proxy_endpoint_control.connect(endpoint).unwrap();
    assert_eq!(
        endpoint_state.lock().stop_endpoint_counter,
        stop_endpoint_counter + 1
    );
    assert!(
        channel
            .read_with(|_| panic!("No packet expected"))
            .await
            .is_err()
    );
    assert!(test_vf_state.is_ready_unchanged());
    assert!(endpoint_state.lock().use_vf.is_none());
}

#[async_test]
async fn link_status_update(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let (mut proxy_endpoint, mut proxy_endpoint_control) = DisconnectableEndpoint::new();
    proxy_endpoint_control.connect(Box::new(endpoint)).unwrap();
    proxy_endpoint.wait_for_endpoint_action().await;

    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(proxy_endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let _: protocol::MessageHeader = reader.read_plain().unwrap();
                let _: protocol::Message4SendVfAssociation = reader.read_plain().unwrap();
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &[],
        })
        .await;

    // Send link down
    TestNicEndpointState::update_link_status(&endpoint_state, [false].as_slice());
    // Verify message.
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_DISCONNECT);

    // Sending the same state as the current is considered a toggle.
    // For example, if the link is down, sending a down is a toggle down->up->down and vice versa.
    // And, there is a time delay in between the transition.
    TestNicEndpointState::update_link_status(&endpoint_state, [false].as_slice());
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_CONNECT);
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION * 2,
        )
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_DISCONNECT);

    // Wait for a little bit and make sure the state has not changed.
    let link_status_msg: Option<rndisprot::IndicateStatus> = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await;
    assert!(link_status_msg.is_none());

    // Send link up
    TestNicEndpointState::update_link_status(&endpoint_state, [true].as_slice());
    // Verify message.
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_CONNECT);

    // Send quick down/up.
    TestNicEndpointState::update_link_status(&endpoint_state, [false, true].as_slice());
    // Verify message.
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_DISCONNECT);
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION * 2,
        )
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_CONNECT);

    // Sending the same state as the current is considered a toggle.
    // For example, if the link is up, sending an up is a toggle up->down->up and vice versa.
    // And, there is a time delay in between the transition.
    TestNicEndpointState::update_link_status(&endpoint_state, [true].as_slice());
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_DISCONNECT);
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION * 2,
        )
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_CONNECT);

    // Wait for a little bit and make sure the state has not changed.
    let link_status_msg: Option<rndisprot::IndicateStatus> = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG)
        .await;
    assert!(link_status_msg.is_none());

    // Request keep alive status but don't process results in order to exhaust buffer IDs.
    const FILL_BUFFER_COUNT: u32 = 16;
    for i in 0..FILL_BUFFER_COUNT {
        channel
            .send_rndis_control_message_no_completion(
                rndisprot::MESSAGE_TYPE_KEEPALIVE_MSG,
                rndisprot::KeepaliveRequest { request_id: i },
                &[],
            )
            .await;
    }

    // Send quick down/up. The link status message will be blocked behind no free buffer IDs.
    TestNicEndpointState::update_link_status(&endpoint_state, [false, true].as_slice());

    // Clear ring buffer to free up buffer IDs and ring buffer space.
    for _ in 0..FILL_BUFFER_COUNT {
        channel
            .read_with(|packet| match packet {
                IncomingPacket::Completion(_) => (),
                _ => panic!("Unexpected data packet"),
            })
            .await
            .expect("completion message");
    }
    for _ in 0..FILL_BUFFER_COUNT {
        let _: rndisprot::KeepaliveComplete = channel
            .read_rndis_control_message(rndisprot::MESSAGE_TYPE_KEEPALIVE_CMPLT)
            .await
            .unwrap();
    }

    // The initial message could not be sent because it was blocked. Wait for it.
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION * 2,
        )
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_DISCONNECT);

    // There should also be a delay before the status returns to up.
    let link_status_msg: Option<rndisprot::IndicateStatus> = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION / 10,
        )
        .await;
    assert!(link_status_msg.is_none());
    // Wait for delayed message
    let link_status_msg: rndisprot::IndicateStatus = channel
        .read_rndis_control_message_with_timeout(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            LINK_DELAY_DURATION * 2,
        )
        .await
        .unwrap();
    assert_eq!(link_status_msg.status, rndisprot::STATUS_MEDIA_CONNECT);
}

#[async_test]
async fn send_rndis_reset_message(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    // Test Reset message. Will result in RndisMessageTypeNotImplemented, but not panic due to unimplemented!().
    // Note: Attempted to use tracing-test crate to check for Error in the trace, but there already exists a global trace dispatcher.
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_RESET_MSG,
            rndisprot::ResetRequest { reserved: 0 },
            &[],
        )
        .await;
}

#[async_test]
async fn send_rndis_indicate_status_message(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    // Test Indicate Status message. Will result in RndisMessageTypeNotImplemented, but not panic due to unimplemented!().
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
            rndisprot::IndicateStatus {
                status: 0,
                status_buffer_length: 0,
                status_buffer_offset: 0,
            },
            &[],
        )
        .await;
}

#[async_test]
async fn send_rndis_set_packet_filter(driver: DefaultDriver) {
    const TOTAL_QUEUES: u32 = 4;
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(
            TOTAL_QUEUES as usize - 1,
            protocol::NdisConfigCapabilities::new().with_sriov(true),
        )
        .await;

    let rndis_parser = channel.rndis_message_parser();

    // Send and verify Initialization
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    // Allocate subchannels
    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE5_TYPE_SUB_CHANNEL,
        },
        data: protocol::Message5SubchannelRequest {
            operation: protocol::SubchannelOperation::ALLOCATE,
            num_sub_channels: TOTAL_QUEUES - 1,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(header.message_type, protocol::MESSAGE5_TYPE_SUB_CHANNEL);
                let completion_data: protocol::Message5SubchannelComplete =
                    reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
                assert_eq!(completion_data.num_sub_channels, TOTAL_QUEUES - 1);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    for idx in 1..TOTAL_QUEUES {
        channel.connect_subchannel(idx).await;
    }

    // Send Indirection Table
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let mut reader = packet.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE
                );
                packet.transaction_id()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("indirection table message after all channels connected");
    if let Some(transaction_id) = transaction_id {
        channel
            .write(OutgoingPacket {
                transaction_id,
                packet_type: OutgoingPacketType::Completion,
                payload: &NvspMessage {
                    header: protocol::MessageHeader {
                        message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                    },
                    data: protocol::Message1SendRndisPacketComplete {
                        status: protocol::Status::SUCCESS,
                    },
                    padding: &[],
                }
                .payload(),
            })
            .await;
    }

    // Send a packet on every queue.
    {
        let locked_state = endpoint_state.lock();
        for (idx, queue) in locked_state.queues.iter().enumerate() {
            queue.send(vec![idx as u8]);
        }
    }

    // Expect no packets
    for idx in 0..TOTAL_QUEUES {
        channel
            .read_subchannel_with(idx, |_| panic!("Unexpected packet on subchannel {}", idx))
            .await
            .expect_err("Packet should have been filtered");
    }

    // Set packet filter
    let request_id = 456;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id,
                oid: rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                information_buffer_length: size_of::<u32>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            &rndisprot::NPROTO_PACKET_FILTER.to_le_bytes(),
        )
        .await;

    let set_complete: rndisprot::SetComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_SET_CMPLT)
        .await
        .unwrap();

    assert_eq!(set_complete.request_id, request_id);
    assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);

    // Send a packet on every queue.
    {
        let locked_state = endpoint_state.lock();
        for (idx, queue) in locked_state.queues.iter().enumerate() {
            queue.send(vec![idx as u8]);
        }
    }

    // Receive and complete the data packets.
    for idx in 0..TOTAL_QUEUES {
        let txid = channel
            .read_subchannel_with(idx, |packet| match packet {
                IncomingPacket::Data(packet) => {
                    let (_, external_ranges) = rndis_parser.parse_data_message(packet);
                    let data: u8 = rndis_parser.get_data_packet_content(&external_ranges);
                    assert_eq!(idx, data as u32);
                    packet
                        .transaction_id()
                        .expect("data packets should have txid")
                }
                _ => panic!("Unexpected packet on subchannel {}", idx),
            })
            .await
            .expect("Data packet");
        channel
            .write_subchannel(
                idx,
                OutgoingPacket {
                    transaction_id: txid,
                    packet_type: OutgoingPacketType::Completion,
                    payload: &NvspMessage {
                        header: protocol::MessageHeader {
                            message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                        },
                        data: protocol::Message1SendRndisPacketComplete {
                            status: protocol::Status::SUCCESS,
                        },
                        padding: &[],
                    }
                    .payload(),
                },
            )
            .await;
    }

    // Set packet filter to None
    let request_id = 789;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id,
                oid: rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                information_buffer_length: size_of::<u32>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            &rndisprot::NDIS_PACKET_TYPE_NONE.to_le_bytes(),
        )
        .await;

    let set_complete: rndisprot::SetComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_SET_CMPLT)
        .await
        .unwrap();

    assert_eq!(set_complete.request_id, request_id);
    assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);

    // Test sending packets with the filter set to None.
    for _ in 0..2 {
        let locked_state = endpoint_state.lock();
        for (idx, queue) in locked_state.queues.iter().enumerate() {
            queue.send(vec![idx as u8]);
        }
    }

    // Expect no packets
    for idx in 0..TOTAL_QUEUES {
        channel
            .read_subchannel_with(idx, |_| panic!("Unexpected packet on subchannel {}", idx))
            .await
            .expect_err("Packet should have been filtered");
    }

    // Set packet filter to receive new packets.
    let request_id = 456;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id,
                oid: rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                information_buffer_length: size_of::<u32>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            &rndisprot::NPROTO_PACKET_FILTER.to_le_bytes(),
        )
        .await;

    let _: rndisprot::SetComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_SET_CMPLT)
        .await
        .unwrap();

    // Expect processing of rx packets to have stopped because of the filter
    // state. For the test queues, this means they should still have pending
    // data, so rx packets should be available.
    for idx in 0..TOTAL_QUEUES {
        channel
            .read_subchannel_with(idx, |_| ())
            .await
            .expect("Data packet");
    }
}

#[async_test]
async fn send_rndis_set_ex_message(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    // Test Set Ex message. Will result in RndisMessageTypeNotImplemented, but not panic due to unimplemented!().
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_EX_MSG,
            rndisprot::SetExRequest {
                request_id: 0,
                oid: rndisprot::Oid(0x00010102),
                information_buffer_length: 0,
                information_buffer_offset: 0,
                device_vc_handle: 0,
            },
            &[],
        )
        .await;
}

#[async_test]
async fn set_rss_parameter(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 1],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [0],
    };
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: size_of::<RssParams>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_params.as_bytes(),
        )
        .await;
    let rndis_parser = channel.rndis_message_parser();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                let set_complete: rndisprot::SetComplete = rndis_parser.get(&external_ranges);
                assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;
}

#[async_test]
async fn set_rss_parameter_no_valid_queues(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 1],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        // There is no queue '1' as there are no subchannels.
        indirection_table: [1],
    };
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: size_of::<RssParams>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_params.as_bytes(),
        )
        .await;
    let rndis_parser = channel.rndis_message_parser();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                let set_complete: rndisprot::SetComplete = rndis_parser.get(&external_ranges);
                assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;
}

// Don't include queue zero in the indirection table. Worker[0] is supposed to
// own the reserved IDs, meaning it is always expected to have an operable
// queue.
#[async_test]
async fn set_rss_parameter_unused_first_queue(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(1, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;

    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE5_TYPE_SUB_CHANNEL,
        },
        data: protocol::Message5SubchannelRequest {
            operation: protocol::SubchannelOperation::ALLOCATE,
            num_sub_channels: 1,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(header.message_type, protocol::MESSAGE5_TYPE_SUB_CHANNEL);
                let completion_data: protocol::Message5SubchannelComplete =
                    reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
                assert_eq!(completion_data.num_sub_channels, 1);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 1],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [1],
    };
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: size_of::<RssParams>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_params.as_bytes(),
        )
        .await;
    let rndis_parser = channel.rndis_message_parser();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                let set_complete: rndisprot::SetComplete = rndis_parser.get(&external_ranges);
                assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");

    channel.connect_subchannel(1).await;

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let mut reader = packet.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE
                );
                let indirection_table_desc: protocol::Message5SendIndirectionTable =
                    reader.read_plain().unwrap();
                let skip_bytes = indirection_table_desc.table_offset as usize
                    - (size_of::<protocol::MessageHeader>()
                        + size_of::<protocol::Message5SendIndirectionTable>());
                assert!(reader.skip(skip_bytes).is_ok());
                let indirection_table: Vec<u32> = reader
                    .read_n(indirection_table_desc.table_entry_count as usize)
                    .unwrap();
                // TODO: Is this supposed to reflect the table we sent?
                for (idx, queue_idx) in indirection_table.iter().enumerate() {
                    assert_eq!(*queue_idx, idx as u32 % 2);
                }
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("indirection table message after all channels connected");

    // Complete the MESSAGE_TYPE_SET_CMPLT packet from earlier.
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;
}

#[async_test]
async fn rndis_send_single_packet_message(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);

    let frame = vec![0xCC; 60];
    channel
        .send_rndis_control_message_no_completion(
            rndisprot::MESSAGE_TYPE_PACKET_MSG,
            rndisprot::Packet {
                data_offset: size_of::<rndisprot::MessageHeader>() as u32,
                data_length: frame.len() as u32,
                oob_data_offset: 0,
                oob_data_length: 0,
                num_oob_data_elements: 0,
                per_packet_info_offset: 0,
                per_packet_info_length: 0,
                vc_handle: 0,
                reserved: 0,
            },
            frame.as_slice(),
        )
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::SUCCESS);
}

#[async_test]
async fn rndis_send_multiple_packet_message(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);

    let extras = vec![vec![0xAA; 20], vec![0xBB; 20]];
    let mut packets = vec![
        rndisprot::Packet {
            data_offset: size_of::<rndisprot::MessageHeader>() as u32,
            data_length: extras[0].len() as u32,
            oob_data_offset: 0,
            oob_data_length: 0,
            num_oob_data_elements: 0,
            per_packet_info_offset: 0,
            per_packet_info_length: 0,
            vc_handle: 0,
            reserved: 0,
        },
        rndisprot::Packet {
            data_offset: size_of::<rndisprot::MessageHeader>() as u32,
            data_length: extras[1].len() as u32,
            oob_data_offset: 0,
            oob_data_length: 0,
            num_oob_data_elements: 0,
            per_packet_info_offset: 0,
            per_packet_info_length: 0,
            vc_handle: 0,
            reserved: 0,
        },
    ];
    channel
        .send_rndis_multiplepacket_no_completion(packets.clone(), extras.clone())
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::SUCCESS);

    // Assign the second RNDIS packet an invalid length
    packets[1].data_length = 0;

    channel
        .send_rndis_multiplepacket_no_completion(packets, extras)
        .await;
    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::FAILURE);
}

// Start with six queues (primary plus five subchannels) each with one receive
// buffer. Send an rx packet on each queue. Then reduce active queues to four,
// such that the total receive buffers (six) does not evenly divide among the
// remaining queues. Complete the packets on the original queues they were
// received to ensure that they are redirected appropriately to the new owning
// queue.
#[async_test]
async fn set_rss_parameter_bufs_not_evenly_divisible(driver: DefaultDriver) {
    const TOTAL_QUEUES: u32 = 6;
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(
            TOTAL_QUEUES as usize - 1,
            protocol::NdisConfigCapabilities::new().with_sriov(true),
        )
        .await;

    let rndis_parser = channel.rndis_message_parser();

    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(_) => (),
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    let message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE5_TYPE_SUB_CHANNEL,
        },
        data: protocol::Message5SubchannelRequest {
            operation: protocol::SubchannelOperation::ALLOCATE,
            num_sub_channels: TOTAL_QUEUES - 1,
        },
        padding: &[],
    };
    channel
        .write(OutgoingPacket {
            transaction_id: 123,
            packet_type: OutgoingPacketType::InBandWithCompletion,
            payload: &message.payload(),
        })
        .await;
    channel
        .read_with(|packet| match packet {
            IncomingPacket::Completion(completion) => {
                let mut reader = completion.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(header.message_type, protocol::MESSAGE5_TYPE_SUB_CHANNEL);
                let completion_data: protocol::Message5SubchannelComplete =
                    reader.read_plain().unwrap();
                assert_eq!(completion_data.status, protocol::Status::SUCCESS);
                assert_eq!(completion_data.num_sub_channels, TOTAL_QUEUES - 1);
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("completion message");

    for idx in 1..TOTAL_QUEUES {
        channel.connect_subchannel(idx).await;
    }

    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let mut reader = packet.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE
                );
                packet.transaction_id()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("indirection table message after all channels connected");
    if let Some(transaction_id) = transaction_id {
        channel
            .write(OutgoingPacket {
                transaction_id,
                packet_type: OutgoingPacketType::Completion,
                payload: &NvspMessage {
                    header: protocol::MessageHeader {
                        message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                    },
                    data: protocol::Message1SendRndisPacketComplete {
                        status: protocol::Status::SUCCESS,
                    },
                    padding: &[],
                }
                .payload(),
            })
            .await;
    }

    // Set packet filter
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                information_buffer_length: size_of::<u32>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            &rndisprot::NPROTO_PACKET_FILTER.to_le_bytes(),
        )
        .await;

    let set_complete: rndisprot::SetComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_SET_CMPLT)
        .await
        .unwrap();

    assert_eq!(set_complete.request_id, 0);
    assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);

    // Receive a packet on every queue.
    {
        let locked_state = endpoint_state.lock();
        for (idx, queue) in locked_state.queues.iter().enumerate() {
            queue.send(vec![idx as u8]);
        }
    }

    // Get the transaction IDs for all of the received packets.
    let mut rx_tx_ids = Vec::new();
    for idx in 0..TOTAL_QUEUES {
        rx_tx_ids.push(
            channel
                .read_subchannel_with(idx, |packet| match packet {
                    IncomingPacket::Data(packet) => {
                        let (_, external_ranges) = rndis_parser.parse_data_message(packet);
                        let data: u8 = rndis_parser.get_data_packet_content(&external_ranges);
                        assert_eq!(idx, data as u32);
                        packet.transaction_id().unwrap()
                    }
                    _ => panic!("Unexpected packet"),
                })
                .await
                .expect("Data packet"),
        );
    }

    // Reduce to four active queues.
    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 4],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4 * size_of::<u32>() as u16,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [0, 1, 2, 3],
    };
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: size_of::<RssParams>() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_params.as_bytes(),
        )
        .await;
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                let set_complete: rndisprot::SetComplete = rndis_parser.get(&external_ranges);
                assert_eq!(set_complete.status, rndisprot::STATUS_SUCCESS);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");
    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;

    // Complete the rx packets on the original six queues.
    for (idx, rx_tx_id) in rx_tx_ids.into_iter().enumerate() {
        tracing::info!(idx, rx_tx_id, "completing receive packet");
        channel
            .write_subchannel(
                idx as u32,
                OutgoingPacket {
                    transaction_id: rx_tx_id,
                    packet_type: OutgoingPacketType::Completion,
                    payload: &NvspMessage {
                        header: protocol::MessageHeader {
                            message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                        },
                        data: protocol::Message1SendRndisPacketComplete {
                            status: protocol::Status::SUCCESS,
                        },
                        padding: &[],
                    }
                    .payload(),
                },
            )
            .await;
    }
}

// The netvsp task coordinator can be interrupted for various reasons:
//     1. A notification from the main worker task.
//     2. A notification from the endpoint or VF control.
//     3. A vmbus operation, like retarget VP.
//
// Each of these will cause processing of the main worker loop and/or
// coordinator processing to restart, while there may be oustanding work in
// flight. Stress some of these restart mechanisms to make sure work is not
// lost and state is maintained properly during these transitions.
#[async_test]
async fn race_coordinator_and_worker_stop_events(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let test_vf = Box::new(TestVirtualFunction::new(Some(123)));
    let test_vf_state = test_vf.state();
    let builder = Nic::builder();
    let nic = builder.virtual_function(test_vf).build(
        &VmTaskDriverSource::new(ThreadDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new().with_sriov(true))
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    let _ = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(data) => {
                let mut reader = data.reader();
                let header: protocol::MessageHeader = reader.read_plain().unwrap();
                assert_eq!(
                    header.message_type,
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                );
                let association_data: protocol::Message4SendVfAssociation =
                    reader.read_plain().unwrap();
                assert_eq!(association_data.vf_allocated, 1);
                assert_eq!(association_data.serial_number, test_vf_state.id().unwrap());
                data.transaction_id().expect("should request completion")
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("association packet");

    assert!(
        test_vf_state
            .await_ready(true, Duration::from_millis(333))
            .await
            .is_ok()
    );

    let link_update_completion_message = NvspMessage {
        header: protocol::MessageHeader {
            message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
        },
        data: protocol::Message1SendRndisPacketComplete {
            status: protocol::Status::SUCCESS,
        },
        padding: &[],
    };

    let rndis_parser = channel.rndis_message_parser();
    endpoint_state.lock().poll_iterations_required = 1;
    for i in 0..25 {
        // Trigger a link update 2/3 of the time. This also will queue a timer,
        // which is another event, as true->true is considered a toggle.
        let link_update = if (i % 3) < 2 {
            TestNicEndpointState::update_link_status(&endpoint_state, [true].as_slice());
            true
        } else {
            false
        };
        // Change the VF availability every other instance.
        if (i % 2) == 0 {
            let is_add = (i % 4) == 0;
            test_vf_state
                .update_id(
                    if is_add { Some(124) } else { None },
                    Some(Duration::from_millis(100)),
                )
                .await
                .unwrap();

            if is_add {
                PolledTimer::new(&driver).sleep(VF_DEVICE_DELAY).await;
            }
        }

        // send switch data path messages
        channel
            .write(OutgoingPacket {
                transaction_id: 123,
                packet_type: OutgoingPacketType::InBandWithCompletion,
                payload: &NvspMessage {
                    header: protocol::MessageHeader {
                        message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                    },
                    data: protocol::Message4SwitchDataPath {
                        active_data_path: protocol::DataPath::SYNTHETIC.0,
                    },
                    padding: &[],
                }
                .payload(),
            })
            .await;
        channel
            .write(OutgoingPacket {
                transaction_id: 123,
                packet_type: OutgoingPacketType::InBandWithCompletion,
                payload: &NvspMessage {
                    header: protocol::MessageHeader {
                        message_type: protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                    },
                    data: protocol::Message4SwitchDataPath {
                        active_data_path: protocol::DataPath::VF.0,
                    },
                    padding: &[],
                }
                .payload(),
            })
            .await;

        let mut extra_packets = i;
        for _ in 0..extra_packets {
            channel
                .send_rndis_control_message_no_completion(
                    rndisprot::MESSAGE_TYPE_KEEPALIVE_MSG,
                    rndisprot::KeepaliveRequest { request_id: i },
                    &[],
                )
                .await;
        }

        // Trigger a retarget VP 2/3 of the time offset with the link update,
        // such that 1/3 times only link update or retarget VP will be
        // triggered.
        if (i % 3) != 1 {
            channel.retarget_vp(i).await;
        }

        if link_update {
            extra_packets += 1;
        }
        loop {
            if extra_packets == 0 {
                break;
            }
            let link_update_id = channel
                .read_with(|packet| match packet {
                    IncomingPacket::Completion(_) => None,
                    IncomingPacket::Data(data) => {
                        let mut reader = data.reader();
                        let header: protocol::MessageHeader = reader.read_plain().unwrap();
                        match header.message_type {
                            protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION => {
                                let association_data: protocol::Message4SendVfAssociation =
                                    reader.read_plain().unwrap();
                                tracing::info!(
                                    is_vf = association_data.vf_allocated,
                                    vfid = association_data.serial_number,
                                    "Message: VF association"
                                );
                            }
                            protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH => {
                                tracing::info!("Message: switch data path");
                                let switch_result: protocol::Message4SwitchDataPath =
                                    reader.read_plain().unwrap();
                                // Switch data path is expected when the data
                                // path is forced to synthetic.
                                assert_eq!(
                                    switch_result.active_data_path,
                                    protocol::DataPath::SYNTHETIC.0
                                );
                            }
                            _ => {
                                let (rndis_header, _) = rndis_parser.parse_control_message(data);
                                if rndis_header.message_type
                                    == rndisprot::MESSAGE_TYPE_KEEPALIVE_CMPLT
                                {
                                    tracing::info!("Message: keepalive completion");
                                } else {
                                    tracing::info!(
                                        rndis_header.message_type,
                                        "Message: link status update"
                                    );
                                }
                            }
                        }
                        Some(data.transaction_id().expect("should request completion"))
                    }
                })
                .await
                .expect("completion message");
            if let Some(transaction_id) = link_update_id {
                channel
                    .write(OutgoingPacket {
                        transaction_id,
                        packet_type: OutgoingPacketType::Completion,
                        payload: &link_update_completion_message.payload(),
                    })
                    .await;
            } else {
                extra_packets -= 1;
            }
        }
    }
}

#[async_test]
async fn rndis_send_lso_packet(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);

    let data = vec![0xCC; 60];
    let tcp_checksum = false;
    let udp_checksum = false;
    let lso = true;
    channel
        .send_rndis_packet_offload(data.as_slice(), tcp_checksum, udp_checksum, lso)
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::SUCCESS);
}

#[async_test]
async fn rndis_send_tcp_checksum_packet(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 123,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;

    let initialize_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();
    assert_eq!(initialize_complete.request_id, 123);
    assert_eq!(initialize_complete.status, rndisprot::STATUS_SUCCESS);
    assert_eq!(initialize_complete.major_version, rndisprot::MAJOR_VERSION);
    assert_eq!(initialize_complete.minor_version, rndisprot::MINOR_VERSION);

    assert_eq!(endpoint_state.lock().stop_endpoint_counter, 1);

    let data = vec![0xCC; 60];
    let tcp_checksum = true;
    let udp_checksum = false;
    let lso = false;
    channel
        .send_rndis_packet_offload(data.as_slice(), tcp_checksum, udp_checksum, lso)
        .await;

    let completion = channel.read_rndis_packet_complete_message().await.unwrap();
    assert_eq!(completion.status, protocol::Status::SUCCESS);
}

/// Helper: builds an RSS-enable parameter block that the set_rss_parameter
/// OID path accepts.
fn build_rss_enable_params() -> Vec<u8> {
    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 1],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: 0,
            base_cpu_number: 0,
            hash_information: rndisprot::NDIS_HASH_FUNCTION_TOEPLITZ,
            indirection_table_size: 4,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [0],
    };
    rss_params.as_bytes().to_vec()
}

/// Helper: builds an RSS-disable parameter block (sets
/// `NDIS_RSS_PARAM_FLAG_DISABLE_RSS`).
fn build_rss_disable_params() -> Vec<u8> {
    #[repr(C)]
    #[derive(FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct RssParams {
        params: rndisprot::NdisReceiveScaleParameters,
        hash_secret_key: [u8; 40],
        indirection_table: [u32; 1],
    }

    let rss_params = RssParams {
        params: rndisprot::NdisReceiveScaleParameters {
            header: rndisprot::NdisObjectHeader {
                object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
                revision: 1,
                size: size_of::<RssParams>() as u16,
            },
            flags: NDIS_RSS_PARAM_FLAG_DISABLE_RSS,
            base_cpu_number: 0,
            hash_information: 0,
            indirection_table_size: 4,
            pad0: 0,
            indirection_table_offset: offset_of!(RssParams, indirection_table) as u32,
            hash_secret_key_size: 40,
            pad1: 0,
            hash_secret_key_offset: offset_of!(RssParams, hash_secret_key) as u32,
            processor_masks_offset: 0,
            number_of_processor_masks: 0,
            processor_masks_entry_size: 0,
            default_processor_number: 0,
        },
        hash_secret_key: [0; 40],
        indirection_table: [0],
    };
    rss_params.as_bytes().to_vec()
}

/// Helper: send an RSS OID SET and consume the SET_CMPLT + completion.
async fn send_rss_oid_and_complete(channel: &mut TestNicChannel<'_>, rss_bytes: &[u8]) {
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_SET_MSG,
            rndisprot::SetRequest {
                request_id: 0,
                oid: rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS,
                information_buffer_length: rss_bytes.len() as u32,
                information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
                device_vc_handle: 0,
            },
            rss_bytes,
        )
        .await;

    let rndis_parser = channel.rndis_message_parser();
    let transaction_id = channel
        .read_with(|packet| match packet {
            IncomingPacket::Data(packet) => {
                let (header, _external_ranges) = rndis_parser.parse_control_message(packet);
                assert_eq!(header.message_type, rndisprot::MESSAGE_TYPE_SET_CMPLT);
                packet.transaction_id().unwrap()
            }
            _ => panic!("Unexpected packet"),
        })
        .await
        .expect("RSS completion message");

    channel
        .write(OutgoingPacket {
            transaction_id,
            packet_type: OutgoingPacketType::Completion,
            payload: &NvspMessage {
                header: protocol::MessageHeader {
                    message_type: protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                },
                data: protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                },
                padding: &[],
            }
            .payload(),
        })
        .await;
}

/// Verifies that in-flight TX packets receive completions after an endpoint
/// restart triggered by an RSS parameter change.
///
/// Uses `TestNicEndpoint` with `sync_tx` set to `false` so queues return
/// `sync=false` from `tx_avail`, leaving TX packets in the
/// `pending_tx_packets` state. When `restart_queues()` stops the endpoint,
/// `reset_tx_after_endpoint_stop()` should queue completions so the guest
/// is unblocked.
#[async_test]
async fn tx_inflight_packets_completed_after_endpoint_restart(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    endpoint_state.lock().sync_tx = false;
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;

    // Send RNDIS init so the device enters the active state.
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 1,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;
    let _init_complete: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    let stop_before = endpoint_state.lock().stop_endpoint_counter;

    // Send a data packet. Because `sync_tx` is false, `tx_avail` returns
    // `(false, N)`, the packet stays in-flight (pending_packet_count > 0).
    let frame = vec![0xBB; 60];
    channel
        .send_rndis_control_message_no_completion(
            rndisprot::MESSAGE_TYPE_PACKET_MSG,
            rndisprot::Packet {
                data_offset: size_of::<rndisprot::Packet>() as u32,
                data_length: frame.len() as u32,
                oob_data_offset: 0,
                oob_data_length: 0,
                num_oob_data_elements: 0,
                per_packet_info_offset: 0,
                per_packet_info_length: 0,
                vc_handle: 0,
                reserved: 0,
            },
            frame.as_slice(),
        )
        .await;

    // The TX is NOT completed synchronously because the mock endpoint is
    // async. A completion should NOT arrive yet.
    // (We don't assert no-completion here because the worker may not have
    // processed the packet yet; instead we proceed to trigger the restart.)

    // Trigger an RSS parameter change, which causes restart_queues() →
    // endpoint.stop() → reset_tx_after_endpoint_stop().
    let rss_bytes = build_rss_enable_params();
    send_rss_oid_and_complete(&mut channel, &rss_bytes).await;

    // The in-flight TX packet should now have a completion queued by
    // reset_tx_after_endpoint_stop(). Read it.
    let completion = channel
        .read_rndis_packet_complete_message_with_timeout(Duration::from_secs(2))
        .await;
    assert!(
        completion.is_some(),
        "Expected TX completion for in-flight packet after endpoint restart"
    );
    assert_eq!(completion.unwrap().status, protocol::Status::SUCCESS);

    // Endpoint should have been stopped once more.
    let stop_after = endpoint_state.lock().stop_endpoint_counter;
    assert!(
        stop_after > stop_before,
        "endpoint.stop() should have been called during restart_queues()"
    );
}

/// Verifies that disabling RSS when RSS is already disabled does NOT trigger
/// a redundant endpoint restart.
///
/// The PR optimizes `handle_oid_set` to skip `restart_endpoint = true` when
/// `!had_rss && primary.rss_state.is_none()`.
#[async_test]
async fn rss_disable_when_already_disabled_skips_endpoint_restart(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;

    // RNDIS Initialize.
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 1,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    // Record stop counter after init (init itself causes one stop).
    let stop_before = endpoint_state.lock().stop_endpoint_counter;

    // RSS is not enabled yet. Send an RSS-disable OID — this should be a
    // no-op that skips the endpoint restart.
    let rss_disable_bytes = build_rss_disable_params();
    send_rss_oid_and_complete(&mut channel, &rss_disable_bytes).await;

    let stop_after = endpoint_state.lock().stop_endpoint_counter;
    assert_eq!(
        stop_before, stop_after,
        "endpoint.stop() should NOT be called when RSS was already disabled"
    );
}

/// Verifies that disabling RSS when RSS was previously enabled DOES trigger
/// an endpoint restart (the skip optimization only applies when both states
/// are disabled).
#[async_test]
async fn rss_enable_then_disable_triggers_endpoint_restart(driver: DefaultDriver) {
    let endpoint_state = TestNicEndpointState::new();
    let endpoint = TestNicEndpoint::new(Some(endpoint_state.clone()));
    let builder = Nic::builder();
    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        Box::new(endpoint),
        [1, 2, 3, 4, 5, 6].into(),
        0,
    );

    let mut nic = TestNicDevice::new_with_nic(&driver, nic).await;
    nic.start_vmbus_channel();
    let mut channel = nic.connect_vmbus_channel().await;
    channel
        .initialize(0, protocol::NdisConfigCapabilities::new())
        .await;

    // RNDIS Initialize.
    channel
        .send_rndis_control_message(
            rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            rndisprot::InitializeRequest {
                request_id: 1,
                major_version: rndisprot::MAJOR_VERSION,
                minor_version: rndisprot::MINOR_VERSION,
                max_transfer_size: 0,
            },
            &[],
        )
        .await;
    let _: rndisprot::InitializeComplete = channel
        .read_rndis_control_message(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
        .await
        .unwrap();

    // Enable RSS first.
    let rss_enable_bytes = build_rss_enable_params();
    send_rss_oid_and_complete(&mut channel, &rss_enable_bytes).await;

    // Record stop counter after RSS enable.
    let stop_before = endpoint_state.lock().stop_endpoint_counter;

    // Now disable RSS — since RSS was previously enabled, this MUST trigger
    // an endpoint restart.
    let rss_disable_bytes = build_rss_disable_params();
    send_rss_oid_and_complete(&mut channel, &rss_disable_bytes).await;

    let stop_after = endpoint_state.lock().stop_endpoint_counter;
    assert!(
        stop_after > stop_before,
        "endpoint.stop() should be called when transitioning from RSS enabled to disabled"
    );
}
