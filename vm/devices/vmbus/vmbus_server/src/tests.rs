// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use inspect::InspectMut;
use mesh::CancelReason;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use protocol::UserDefinedData;
use std::time::Duration;
use test_with_tracing::test;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::channel::DeviceResources;
use vmbus_channel::channel::SaveRestoreVmbusDevice;
use vmbus_channel::channel::VmbusDevice;
use vmbus_channel::channel::offer_channel;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::VmbusMessage;
use vmcore::synic::MonitorInfo;
use vmcore::synic::SynicMonitorAccess;
use vmcore::synic::SynicPortAccess;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

struct MockSynicInner {
    message_port: Option<Arc<dyn MessagePort>>,
    monitor_page: Option<MonitorPageGpas>,
}

struct MockSynic {
    inner: Mutex<MockSynicInner>,
    message_send: mesh::Sender<Vec<u8>>,
    spawner: DefaultDriver,
    allow_allocated_monitor_pages: bool,
}

impl MockSynic {
    fn new(
        message_send: mesh::Sender<Vec<u8>>,
        spawner: DefaultDriver,
        allow_allocated_monitor_pages: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(MockSynicInner {
                message_port: None,
                monitor_page: None,
            }),
            message_send,
            spawner,
            allow_allocated_monitor_pages,
        }
    }

    fn send_message(&self, msg: impl VmbusMessage + IntoBytes + Immutable + KnownLayout) {
        self.send_message_core(OutgoingMessage::new(&msg), false);
    }

    fn send_message_trusted(&self, msg: impl VmbusMessage + IntoBytes + Immutable + KnownLayout) {
        self.send_message_core(OutgoingMessage::new(&msg), true);
    }

    fn send_message_core(&self, msg: OutgoingMessage, trusted: bool) {
        assert_eq!(
            self.inner
                .lock()
                .message_port
                .as_ref()
                .unwrap()
                .poll_handle_message(
                    &mut std::task::Context::from_waker(std::task::Waker::noop()),
                    msg.data(),
                    trusted,
                ),
            Poll::Ready(())
        );
    }
}

#[derive(Debug)]
struct MockGuestPort {}

impl GuestEventPort for MockGuestPort {
    fn interrupt(&self) -> Interrupt {
        Interrupt::null()
    }

    fn set_target_vp(&mut self, _vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        Ok(())
    }
}

struct MockGuestMessagePort {
    send: mesh::Sender<Vec<u8>>,
    spawner: DefaultDriver,
    timer: Option<(PolledTimer, Instant)>,
}

impl GuestMessagePort for MockGuestMessagePort {
    fn poll_post_message(
        &mut self,
        cx: &mut std::task::Context<'_>,
        _typ: u32,
        payload: &[u8],
    ) -> Poll<()> {
        if let Some((timer, deadline)) = self.timer.as_mut() {
            ready!(timer.sleep_until(*deadline).poll_unpin(cx));
            self.timer = None;
        }

        // Return pending 25% of the time.
        let mut pending_chance = [0; 1];
        getrandom::fill(&mut pending_chance).unwrap();
        if pending_chance[0] % 4 == 0 {
            let mut timer = PolledTimer::new(&self.spawner);
            let deadline = Instant::now() + Duration::from_millis(10);
            match timer.sleep_until(deadline).poll_unpin(cx) {
                Poll::Ready(_) => {}
                Poll::Pending => {
                    self.timer = Some((timer, deadline));
                    return Poll::Pending;
                }
            }
        }

        self.send.send(payload.into());
        Poll::Ready(())
    }

    fn set_target_vp(&mut self, _vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        Ok(())
    }
}

impl Inspect for MockGuestMessagePort {
    fn inspect(&self, _req: inspect::Request<'_>) {}
}

impl SynicPortAccess for MockSynic {
    fn add_message_port(
        &self,
        connection_id: u32,
        _minimum_vtl: Vtl,
        port: Arc<dyn MessagePort>,
    ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
        self.inner.lock().message_port = Some(port);
        Ok(Box::new(connection_id))
    }

    fn add_event_port(
        &self,
        connection_id: u32,
        _minimum_vtl: Vtl,
        _port: Arc<dyn EventPort>,
        _monitor_info: Option<MonitorInfo>,
    ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
        Ok(Box::new(connection_id))
    }

    fn new_guest_message_port(
        &self,
        _vtl: Vtl,
        _vp: u32,
        _sint: u8,
    ) -> Result<Box<dyn GuestMessagePort>, vmcore::synic::HypervisorError> {
        Ok(Box::new(MockGuestMessagePort {
            send: self.message_send.clone(),
            spawner: self.spawner.clone(),
            timer: None,
        }))
    }

    fn new_guest_event_port(
        &self,
        _port_id: u32,
        _vtl: Vtl,
        _vp: u32,
        _sint: u8,
        _flag: u16,
        _monitor_info: Option<MonitorInfo>,
    ) -> Result<Box<dyn GuestEventPort>, vmcore::synic::HypervisorError> {
        Ok(Box::new(MockGuestPort {}))
    }

    fn prefer_os_events(&self) -> bool {
        false
    }

    fn monitor_support(&self) -> Option<&dyn SynicMonitorAccess> {
        Some(self)
    }
}

impl SynicMonitorAccess for MockSynic {
    fn set_monitor_page(&self, vtl: Vtl, gpa: Option<MonitorPageGpas>) -> anyhow::Result<()> {
        assert_eq!(vtl, Vtl::Vtl0);
        let mut inner = self.inner.lock();
        inner.monitor_page = gpa;
        Ok(())
    }

    fn allocate_monitor_page(&self, vtl: Vtl) -> anyhow::Result<Option<MonitorPageGpas>> {
        assert_eq!(vtl, Vtl::Vtl0);
        if !self.allow_allocated_monitor_pages {
            return Ok(None);
        }

        let gpas = MonitorPageGpas {
            child_to_parent: 0x123000,
            parent_to_child: 0x321000,
        };
        let mut inner = self.inner.lock();
        inner.monitor_page = Some(gpas);
        Ok(Some(gpas))
    }
}

struct TestChannel {
    request_recv: mesh::Receiver<ChannelRequest>,
    server_request_send: mesh::Sender<ChannelServerRequest>,
    _resources: OfferResources,
}

impl TestChannel {
    async fn next_request(&mut self) -> ChannelRequest {
        self.request_recv.next().await.unwrap()
    }

    async fn handle_gpadl(&mut self) {
        let ChannelRequest::Gpadl(rpc) = self.next_request().await else {
            panic!("Wrong request");
        };

        rpc.complete(true);
    }

    async fn handle_open(&mut self, f: fn(&OpenRequest)) {
        let ChannelRequest::Open(rpc) = self.next_request().await else {
            panic!("Wrong request");
        };

        f(rpc.input());
        rpc.complete(true);
    }

    async fn handle_gpadl_teardown(&mut self) {
        let rpc = self.get_gpadl_teardown().await;
        rpc.complete(());
    }

    async fn get_gpadl_teardown(&mut self) -> Rpc<GpadlId, ()> {
        let ChannelRequest::TeardownGpadl(rpc) = self.next_request().await else {
            panic!("Wrong request");
        };

        rpc
    }

    async fn restore(&self) {
        self.server_request_send
            .call(ChannelServerRequest::Restore, false)
            .await
            .unwrap()
            .unwrap();
    }
}

struct TestEnvBuilder {
    spawner: DefaultDriver,
    allow_allocated_monitor_pages: bool,
}

impl TestEnvBuilder {
    fn new(spawner: DefaultDriver) -> Self {
        Self {
            spawner,
            allow_allocated_monitor_pages: false,
        }
    }

    fn allow_allocated_monitor_pages(mut self, allow: bool) -> Self {
        self.allow_allocated_monitor_pages = allow;
        self
    }

    fn build(self) -> TestEnv {
        let (message_send, message_recv) = mesh::channel();
        let synic = Arc::new(MockSynic::new(
            message_send,
            self.spawner.clone(),
            self.allow_allocated_monitor_pages,
        ));
        let gm = GuestMemory::empty();
        let vmbus = VmbusServerBuilder::new(self.spawner, synic.clone(), gm)
            .enable_mnf(true)
            .build()
            .unwrap();

        TestEnv {
            vmbus,
            synic,
            message_recv,
            trusted: false,
        }
    }
}

struct TestEnv {
    vmbus: VmbusServer,
    synic: Arc<MockSynic>,
    message_recv: mesh::Receiver<Vec<u8>>,
    trusted: bool,
}

impl TestEnv {
    fn new(spawner: DefaultDriver) -> Self {
        TestEnvBuilder::new(spawner).build()
    }

    async fn offer(&self, id: u32, allow_confidential_external_memory: bool) -> TestChannel {
        let guid = Guid {
            data1: id,
            ..Guid::ZERO
        };
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();
        let offer = OfferInput {
            event: Interrupt::from_fn(|| {}),
            request_send,
            server_request_recv,
            params: OfferParams {
                interface_name: "test".into(),
                instance_id: guid,
                interface_id: guid,
                mmio_megabytes: 0,
                mmio_megabytes_optional: 0,
                channel_type: vmbus_channel::bus::ChannelType::Device {
                    pipe_packets: false,
                },
                subchannel_index: 0,
                mnf_interrupt_latency: None,
                offer_order: None,
                allow_confidential_external_memory,
            },
        };

        let control = self.vmbus.control();
        let _resources = control.add_child(offer).await.unwrap();

        TestChannel {
            request_recv,
            server_request_send,
            _resources,
        }
    }

    async fn gpadl(&mut self, channel_id: u32, gpadl_id: u32, channel: &mut TestChannel) {
        self.synic.send_message_core(
            OutgoingMessage::with_data(
                &protocol::GpadlHeader {
                    channel_id: ChannelId(channel_id),
                    gpadl_id: GpadlId(gpadl_id),
                    count: 1,
                    len: 16,
                },
                [1u64, 0u64].as_bytes(),
            ),
            self.trusted,
        );

        channel.handle_gpadl().await;
        self.expect_response(protocol::MessageType::GPADL_CREATED)
            .await;
    }

    async fn open_channel(
        &mut self,
        channel_id: u32,
        ring_gpadl_id: u32,
        channel: &mut TestChannel,
        f: fn(&OpenRequest),
    ) {
        self.gpadl(channel_id, ring_gpadl_id, channel).await;
        self.synic.send_message_core(
            OutgoingMessage::new(&protocol::OpenChannel {
                channel_id: ChannelId(channel_id),
                open_id: 0,
                ring_buffer_gpadl_id: GpadlId(ring_gpadl_id),
                target_vp: 0,
                downstream_ring_buffer_page_offset: 0,
                user_data: UserDefinedData::default(),
            }),
            self.trusted,
        );

        channel.handle_open(f).await;
        self.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT)
            .await;
    }

    async fn expect_response(&mut self, expected: protocol::MessageType) {
        let data = self.message_recv.next().await.unwrap();
        let header = protocol::MessageHeader::read_from_prefix(&data).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        assert_eq!(expected, header.message_type())
    }

    async fn get_response<T: VmbusMessage + FromBytes + Immutable + KnownLayout>(&mut self) -> T {
        let data = self.message_recv.next().await.unwrap();
        let (header, message) = protocol::MessageHeader::read_from_prefix(&data).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
        assert_eq!(T::MESSAGE_TYPE, header.message_type());
        T::read_from_prefix(message).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    fn initiate_contact(
        &mut self,
        version: protocol::Version,
        feature_flags: protocol::FeatureFlags,
        trusted: bool,
        use_mnf: bool,
    ) {
        self.synic.send_message_core(
            OutgoingMessage::new(&protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: 0,
                child_to_parent_monitor_page_gpa: if use_mnf { 0x123f000 } else { 0 },
                parent_to_child_monitor_page_gpa: if use_mnf { 0x321f000 } else { 0 },
                interrupt_page_or_target_info: protocol::TargetInfo::new()
                    .with_sint(2)
                    .with_vtl(0)
                    .with_feature_flags(feature_flags.into())
                    .into(),
            }),
            trusted,
        );

        self.trusted = trusted;
    }

    async fn connect(
        &mut self,
        offer_count: u32,
        feature_flags: protocol::FeatureFlags,
        trusted: bool,
    ) {
        self.initiate_contact(protocol::Version::Copper, feature_flags, trusted, false);

        self.expect_response(protocol::MessageType::VERSION_RESPONSE)
            .await;

        self.synic
            .send_message_core(OutgoingMessage::new(&protocol::RequestOffers {}), trusted);

        for _ in 0..offer_count {
            self.expect_response(protocol::MessageType::OFFER_CHANNEL)
                .await;
        }

        self.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
            .await;
    }
}

#[async_test]
async fn test_save_restore(spawner: DefaultDriver) {
    // Most save/restore state is tested in mod channels::tests; this test specifically checks
    // that ServerTaskInner correctly handles some aspects of the save/restore.
    //
    // If this test fails, it is more likely to hang than panic.
    let mut env = TestEnv::new(spawner);
    let mut channel = env.offer(1, false).await;
    env.vmbus.start();
    env.connect(1, protocol::FeatureFlags::new(), false).await;

    // Create a GPADL for the channel.
    env.gpadl(1, 10, &mut channel).await;

    // Start tearing it down.
    env.synic.send_message(protocol::GpadlTeardown {
        channel_id: ChannelId(1),
        gpadl_id: GpadlId(10),
    });

    // Wait for the teardown request here to make sure the server has processed the teardown
    // message, but do not complete it before saving.
    let rpc = channel.get_gpadl_teardown().await;
    env.vmbus.stop().await;
    let saved_state = env.vmbus.save().await;
    env.vmbus.start();

    // Finish tearing down the gpadl and release the channel so the server can reset.
    rpc.complete(());
    env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
        .await;

    env.synic.send_message(protocol::RelIdReleased {
        channel_id: ChannelId(1),
    });

    env.vmbus.reset().await;
    env.vmbus.stop().await;

    // When restoring with a gpadl in the TearingDown state, the teardown request for the device
    // will be repeated. This must not panic.
    env.vmbus.restore(saved_state).await.unwrap();
    channel.restore().await;
    env.vmbus.start();

    // Handle the teardown after restore.
    channel.handle_gpadl_teardown().await;
    env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
        .await;

    env.synic.send_message(protocol::RelIdReleased {
        channel_id: ChannelId(1),
    });
}

struct TestDeviceState {
    id: u32,
    started: bool,
    resources: Option<DeviceResources>,
    open_requests: HashMap<u16, OpenRequest>,
    target_vps: HashMap<u16, u32>,
}

impl TestDeviceState {
    pub fn id(this: &Arc<Mutex<Self>>) -> u32 {
        this.lock().id
    }

    pub fn started(this: &Arc<Mutex<Self>>) -> bool {
        this.lock().started
    }
    pub fn set_started(this: &Arc<Mutex<Self>>, started: bool) {
        this.lock().started = started;
    }

    pub fn open_request(this: &Arc<Mutex<Self>>, channel_idx: u16) -> Option<OpenRequest> {
        this.lock().open_requests.get(&channel_idx).cloned()
    }
    pub fn set_open_request(this: &Arc<Mutex<Self>>, channel_idx: u16, open_request: OpenRequest) {
        assert!(
            this.lock()
                .open_requests
                .insert(channel_idx, open_request)
                .is_none()
        );
    }
    pub fn remove_open_request(this: &Arc<Mutex<Self>>, channel_idx: u16) -> Option<OpenRequest> {
        this.lock().open_requests.remove(&channel_idx)
    }

    pub fn target_vp(this: &Arc<Mutex<Self>>, channel_idx: u16) -> Option<u32> {
        this.lock().target_vps.get(&channel_idx).copied()
    }
    pub fn set_target_vp(this: &Arc<Mutex<Self>>, channel_idx: u16, target_vp: u32) {
        let _ = this.lock().target_vps.insert(channel_idx, target_vp);
    }
}

#[derive(InspectMut)]
struct TestDevice {
    #[inspect(skip)]
    pub state: Arc<Mutex<TestDeviceState>>,
}

impl TestDevice {
    pub fn new_and_state(id: u32) -> (Self, Arc<Mutex<TestDeviceState>>) {
        let state = TestDeviceState {
            id,
            resources: None,
            open_requests: HashMap::new(),
            target_vps: HashMap::new(),
            started: false,
        };
        let state = Arc::new(Mutex::new(state));
        let this = Self {
            state: state.clone(),
        };
        (this, state)
    }
}

#[async_trait]
impl VmbusDevice for TestDevice {
    fn offer(&self) -> OfferParams {
        let guid = Guid {
            data1: TestDeviceState::id(&self.state),
            ..Guid::ZERO
        };

        OfferParams {
            interface_name: "test".into(),
            instance_id: guid,
            interface_id: guid,
            channel_type: vmbus_channel::bus::ChannelType::Device {
                pipe_packets: false,
            },
            ..Default::default()
        }
    }

    fn max_subchannels(&self) -> u16 {
        0
    }

    fn install(&mut self, resources: DeviceResources) {
        self.state.lock().resources = Some(resources);
    }

    async fn open(
        &mut self,
        channel_idx: u16,
        open_request: &OpenRequest,
    ) -> Result<(), ChannelOpenError> {
        tracing::info!("OPEN");
        TestDeviceState::set_open_request(&self.state, channel_idx, open_request.clone());
        Ok(())
    }

    async fn close(&mut self, channel_idx: u16) {
        tracing::info!("CLOSE");
        assert!(TestDeviceState::remove_open_request(&self.state, channel_idx).is_some());
    }

    async fn retarget_vp(&mut self, channel_idx: u16, target_vp: u32) {
        TestDeviceState::set_target_vp(&self.state, channel_idx, target_vp);
    }

    fn start(&mut self) {
        tracing::info!("START");
        TestDeviceState::set_started(&self.state, true);
    }

    async fn stop(&mut self) {
        tracing::info!("STOP");
        TestDeviceState::set_started(&self.state, false);
    }

    fn supports_save_restore(&mut self) -> Option<&mut dyn SaveRestoreVmbusDevice> {
        None
    }
}

#[async_test]
async fn test_stopped_child(spawner: DefaultDriver) {
    // This is mostly testing vmbus_channel behavior when a channel is
    // stopped but vbmus_server is not and continues to receive
    // messages.
    let mut env = TestEnv::new(spawner.clone());
    let (test_device, test_device_state) = TestDevice::new_and_state(1);
    let control = env.vmbus.control();
    let channel = offer_channel(&spawner, control.as_ref(), test_device)
        .await
        .expect("test device failed to offer");

    env.vmbus.start();
    env.connect(1, protocol::FeatureFlags::new(), false).await;

    // Stop the channel.
    channel.stop().await;

    assert_eq!(TestDeviceState::started(&test_device_state), false);

    // GPADL processing is currently allowed while the channel is stopped,
    // so this should complete.
    env.synic.send_message_core(
        OutgoingMessage::with_data(
            &protocol::GpadlHeader {
                channel_id: ChannelId(1),
                gpadl_id: GpadlId(1),
                count: 1,
                len: 16,
            },
            [1u64, 0u64].as_bytes(),
        ),
        false,
    );
    env.expect_response(protocol::MessageType::GPADL_CREATED)
        .await;

    // Open will pend while the channel is stopped.
    env.synic.send_message_core(
        OutgoingMessage::new(&protocol::OpenChannel {
            channel_id: ChannelId(1),
            open_id: 0,
            ring_buffer_gpadl_id: GpadlId(1),
            target_vp: 0,
            downstream_ring_buffer_page_offset: 0,
            user_data: UserDefinedData::default(),
        }),
        false,
    );
    let wait_for_response = mesh::CancelContext::new()
        .with_timeout(Duration::from_millis(150))
        .until_cancelled(env.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT))
        .await;
    assert!(matches!(
        wait_for_response,
        Err(CancelReason::DeadlineExceeded)
    ));
    assert!(TestDeviceState::open_request(&test_device_state, 0).is_none());

    // Restart the channel and confirm that open completes.
    channel.start();
    env.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT)
        .await;
    assert!(TestDeviceState::open_request(&test_device_state, 0).is_some());

    // Stop the channel and send a modify request.
    assert!(TestDeviceState::target_vp(&test_device_state, 0).is_none());
    channel.stop().await;
    env.synic.send_message_core(
        OutgoingMessage::new(&protocol::ModifyChannel {
            channel_id: ChannelId(1),
            target_vp: 2,
        }),
        false,
    );
    let wait_for_response = mesh::CancelContext::new()
        .with_timeout(Duration::from_millis(150))
        .until_cancelled(env.expect_response(protocol::MessageType::MODIFY_CHANNEL_RESPONSE))
        .await;
    assert!(matches!(
        wait_for_response,
        Err(CancelReason::DeadlineExceeded)
    ));

    // Restart the channel and verify the modify request completes.
    channel.start();
    env.expect_response(protocol::MessageType::MODIFY_CHANNEL_RESPONSE)
        .await;
    assert_eq!(
        TestDeviceState::target_vp(&test_device_state, 0).expect("Modify channel request received"),
        2
    );

    // Stop the channel and send a close request. Close is currently
    // allowed through in order to support reset of the vmbus
    // server, so try that.
    channel.stop().await;
    env.vmbus.reset().await;
    assert!(TestDeviceState::open_request(&test_device_state, 0).is_none());

    env.vmbus.stop().await;
}

#[async_test]
async fn test_confidential_connection(spawner: DefaultDriver) {
    let mut env = TestEnv::new(spawner);
    // Add regular bus child channels, one of which supports confidential external memory.
    let mut channel = env.offer(1, false).await;
    let mut channel2 = env.offer(2, true).await;

    // Add a channel directly, like the relay would do.
    let (request_send, request_recv) = mesh::channel();
    let (server_request_send, server_request_recv) = mesh::channel();
    let id = Guid {
        data1: 3,
        ..Guid::ZERO
    };
    let control = env.vmbus.control();
    let relay_resources = control
        .offer_core(OfferInfo {
            params: OfferParamsInternal {
                interface_name: "test".into(),
                instance_id: id,
                interface_id: id,
                mmio_megabytes: 0,
                mmio_megabytes_optional: 0,
                subchannel_index: 0,
                use_mnf: MnfUsage::Disabled,
                offer_order: None,
                flags: protocol::OfferFlags::new().with_enumerate_device_interface(true),
                ..Default::default()
            },
            event: Interrupt::from_fn(|| {}),
            request_send,
            server_request_recv,
        })
        .await
        .unwrap();

    let mut relay_channel = TestChannel {
        request_recv,
        server_request_send,
        _resources: relay_resources,
    };

    env.vmbus.start();
    env.initiate_contact(
        protocol::Version::Copper,
        protocol::FeatureFlags::new().with_confidential_channels(true),
        true,
        false,
    );

    env.expect_response(protocol::MessageType::VERSION_RESPONSE)
        .await;

    env.synic.send_message_trusted(protocol::RequestOffers {});

    // All offers added with add_child have confidential ring support.
    let offer = env.get_response::<protocol::OfferChannel>().await;
    assert!(offer.flags.confidential_ring_buffer());
    assert!(!offer.flags.confidential_external_memory());
    let offer = env.get_response::<protocol::OfferChannel>().await;
    assert!(offer.flags.confidential_ring_buffer());
    assert!(offer.flags.confidential_external_memory());

    // The "relay" channel will not have its flags modified.
    let offer = env.get_response::<protocol::OfferChannel>().await;
    assert!(!offer.flags.confidential_ring_buffer());
    assert!(!offer.flags.confidential_external_memory());

    env.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
        .await;

    // Make sure that the correct confidential flags are set in the open request when opening
    // the channels.
    env.open_channel(1, 1, &mut channel, |request| {
        assert!(request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;

    env.open_channel(2, 2, &mut channel2, |request| {
        assert!(request.use_confidential_ring);
        assert!(request.use_confidential_external_memory);
    })
    .await;

    env.open_channel(3, 3, &mut relay_channel, |request| {
        assert!(!request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;
}

#[async_test]
async fn test_confidential_channels_unsupported(spawner: DefaultDriver) {
    let mut env = TestEnv::new(spawner);
    let mut channel = env.offer(1, false).await;
    let mut channel2 = env.offer(2, true).await;

    env.vmbus.start();
    env.connect(2, protocol::FeatureFlags::new(), true).await;

    // Make sure that the correct confidential flags are always false when the client doesn't
    // support confidential channels.
    env.open_channel(1, 1, &mut channel, |request| {
        assert!(!request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;

    env.open_channel(2, 2, &mut channel2, |request| {
        assert!(!request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;
}

#[async_test]
async fn test_confidential_channels_untrusted(spawner: DefaultDriver) {
    let mut env = TestEnv::new(spawner);
    let mut channel = env.offer(1, false).await;
    let mut channel2 = env.offer(2, true).await;

    env.vmbus.start();
    // Client claims to support confidential channels, but they can't be used because the
    // connection is untrusted.
    env.connect(
        2,
        protocol::FeatureFlags::new().with_confidential_channels(true),
        false,
    )
    .await;

    // Make sure that the correct confidential flags are always false when the client doesn't
    // support confidential channels.
    env.open_channel(1, 1, &mut channel, |request| {
        assert!(!request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;

    env.open_channel(2, 2, &mut channel2, |request| {
        assert!(!request.use_confidential_ring);
        assert!(!request.use_confidential_external_memory);
    })
    .await;
}

#[async_test]
async fn test_server_monitor_page(spawner: DefaultDriver) {
    // Guest pages supplied, but overridden by server pages.
    test_server_monitor_page_helper(spawner.clone(), true, true).await;

    // No guest pages supplied, server will allocate them.
    test_server_monitor_page_helper(spawner.clone(), false, true).await;

    // Server can't allocate pages, so guest pages are used regardless of feature flag.
    test_server_monitor_page_helper(spawner, true, false).await;
}

async fn test_server_monitor_page_helper(
    spawner: DefaultDriver,
    supply_guest_pages: bool,
    allow_allocated_monitor_pages: bool,
) {
    let mut env = TestEnvBuilder::new(spawner)
        .allow_allocated_monitor_pages(allow_allocated_monitor_pages)
        .build();
    env.vmbus.start();
    env.initiate_contact(
        protocol::Version::Copper,
        protocol::FeatureFlags::new().with_server_specified_monitor_pages(true),
        false,
        supply_guest_pages,
    );

    if allow_allocated_monitor_pages {
        let response: protocol::VersionResponse3 = env.get_response().await;
        let flags =
            protocol::FeatureFlags::from_bits(response.version_response2.supported_features);
        assert!(flags.server_specified_monitor_pages());
        assert_eq!(response.parent_to_child_monitor_page_gpa, 0x321000);
        assert_eq!(response.child_to_parent_monitor_page_gpa, 0x123000);
        assert_eq!(
            env.synic.inner.lock().monitor_page,
            Some(MonitorPageGpas {
                parent_to_child: 0x321000,
                child_to_parent: 0x123000,
            })
        );
    } else {
        // Server must not send the feature flag if it doesn't support allocating pages.
        let response: protocol::VersionResponse2 = env.get_response().await;
        let flags = protocol::FeatureFlags::from_bits(response.supported_features);
        assert!(!flags.server_specified_monitor_pages());
        if supply_guest_pages {
            assert_eq!(
                env.synic.inner.lock().monitor_page,
                Some(MonitorPageGpas {
                    child_to_parent: 0x123f000,
                    parent_to_child: 0x321f000,
                })
            );
        } else {
            assert!(env.synic.inner.lock().monitor_page.is_none());
        }
    }
}
