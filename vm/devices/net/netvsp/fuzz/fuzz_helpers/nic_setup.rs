// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NIC setup, teardown, and subchannel opener for fuzz targets.
//!
//! This module wires together the mock VMBus, endpoint, and VF to create
//! a fully functional NIC device on a mock VMBus channel.

use super::PageLayout;
use super::RECV_BUF_PAGES;
use super::RING_PAGES;
use super::endpoint::FuzzEndpoint;
use super::endpoint::FuzzEndpointConfig;
use super::vmbus::FuzzMockVmbus;
use super::vmbus::MultiChannelMockVmbus;
use guestmem::GuestMemory;
use guid::Guid;
use hvdef::hypercall::HvGuestOsId;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use net_backend::Endpoint;
use netvsp::Nic;
use netvsp::VirtualFunction;
use netvsp::test_helpers::gpadl_test_guest_channel;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use vmbus_async::queue::Queue;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::channel::ChannelHandle;
use vmbus_channel::channel::offer_channel;
use vmbus_channel::gpadl::GpadlId;
use vmbus_channel::gpadl::GpadlMap;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::PAGE_SIZE;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::interrupt::Interrupt;
use vmcore::slim_event::SlimEvent;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;

/// Result of setting up a NIC on a mock VMBus, ready for fuzzing.
pub struct FuzzNicSetup {
    pub queue: Queue<GpadlRingMem>,
    /// Guest memory backing the VMBus ring and buffers.
    pub mem: GuestMemory,
    pub recv_buf_gpadl_id: GpadlId,
    pub send_buf_gpadl_id: GpadlId,
    /// Present when `max_subchannels > 0` in [`FuzzNicConfig`]. Call
    /// [`SubchannelOpener::open_pending`] after sending a subchannel
    /// allocation request and draining the primary queue to open the
    /// resulting subchannel rings.
    pub subchannel_opener: Option<SubchannelOpener>,
}

/// GPADL allocation configuration for a single buffer region.
struct GpadlAlloc {
    gpadl_id: GpadlId,
    pages: Vec<u64>,
}

pub struct FuzzNicConfig {
    /// The network endpoint to use.
    /// Either `LoopbackEndpoint` or `FuzzEndpoint`.
    pub endpoint: Box<dyn Endpoint>,
    /// Optional virtual function for VF state fuzzing. Default: None.
    pub virtual_function: Option<Box<dyn VirtualFunction>>,
    /// Optional guest OS ID for exercising `can_use_ring_opt` and related
    /// guest-OS-specific code paths. Default: None.
    pub get_guest_os_id: Option<HvGuestOsId>,
    /// Maximum number of subchannels the device may open. When non-zero, a
    /// [`MultiChannelMockVmbus`] is used and a [`SubchannelOpener`] is placed
    /// in [`FuzzNicSetup`] so the fuzz loop can open real subchannel rings.
    /// Default: 0 (single-channel mode).
    pub max_subchannels: u16,
}

impl Default for FuzzNicConfig {
    fn default() -> Self {
        use hvdef::hypercall::HvGuestOsOpenSource;
        use hvdef::hypercall::HvGuestOsOpenSourceType;

        // Default to Linux 3.11 (version 199424) which is at the
        // can_use_ring_opt threshold, exercising guest-OS-specific code.
        let os_id = HvGuestOsOpenSource::new()
            .with_is_open_source(true)
            .with_os_type(HvGuestOsOpenSourceType::LINUX.0)
            .with_version(199424)
            .with_build_no(0);
        Self {
            endpoint: Box::new(FuzzEndpoint::new(FuzzEndpointConfig::default()).0),
            virtual_function: None,
            get_guest_os_id: Some(HvGuestOsId::from(u64::from(os_id))),
            max_subchannels: 0,
        }
    }
}

/// Internal state produced during NIC setup. Consumed by the two public
/// entry-points: [`setup_fuzz_nic_with_config`] and
/// [`create_nic_with_channel`].
struct NicInternals {
    channel: ChannelHandle<Nic>,
    offer_input: OfferInput,
    setup: FuzzNicSetup,
    subchannel_opened: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    pending_offers: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
}

/// Allocate guest memory, register GPADLs, open the primary channel ring, and
/// return all the pieces needed to either run a normal fuzz loop or call
/// `save`/`restore` on the channel device.
async fn build_nic_internals(
    driver: &pal_async::DefaultDriver,
    layout: &PageLayout,
    config: FuzzNicConfig,
) -> anyhow::Result<NicInternals> {
    let max_subchannels = config.max_subchannels;
    let total_guest_pages = layout.total_pages() + RING_PAGES * max_subchannels as usize;
    let recv_buf_page_count = RECV_BUF_PAGES;
    let send_buf_page_count = layout.send_buf_pages;
    let mock_vmbus = MultiChannelMockVmbus::new(total_guest_pages);
    let mem = mock_vmbus.memory.clone();
    let subchannel_pending = mock_vmbus.pending_offers_arc();
    let subchannel_opened: Arc<futures::lock::Mutex<Vec<OfferInput>>> =
        Arc::new(futures::lock::Mutex::new(Vec::new()));

    let fuzz_vmbus = FuzzMockVmbus::new(mock_vmbus.clone(), driver.clone());

    let mut builder = Nic::builder();
    if let Some(vf) = config.virtual_function {
        builder = builder.virtual_function(vf);
    }
    if let Some(os_id) = config.get_guest_os_id {
        builder = builder.get_guest_os_id(Box::new(move || os_id));
    }

    let nic = builder.build(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        Guid::new_random(),
        config.endpoint,
        [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF].into(),
        0,
    );

    let channel = offer_channel(driver, &fuzz_vmbus, nic)
        .await
        .expect("offer_channel failed");

    let offer_input = {
        let mut offers = mock_vmbus.drain_pending_offers().await;
        assert!(!offers.is_empty(), "no primary offer input from VMBus");
        offers.remove(0)
    };

    let gpadl_map = GpadlMap::new();
    let mut next_page = 0usize;
    let mut next_gpadl_id = 1u32;

    let alloc_gpadl =
        |page_count: usize, next_page: &mut usize, next_gpadl_id: &mut u32| -> GpadlAlloc {
            let gpadl_id = GpadlId(*next_gpadl_id);
            *next_gpadl_id += 1;
            let pages: Vec<u64> = std::iter::once((page_count * PAGE_SIZE) as u64)
                .chain((*next_page..*next_page + page_count).map(|p| p as u64))
                .collect();
            *next_page += page_count;
            GpadlAlloc { gpadl_id, pages }
        };

    // Ring buffer (4 pages).
    let ring = alloc_gpadl(4, &mut next_page, &mut next_gpadl_id);
    let ring_gpadl_id = ring.gpadl_id;
    assert!(
        offer_input
            .request_send
            .call(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: ring.gpadl_id,
                    count: 1,
                    buf: ring.pages.clone(),
                },
            )
            .await
            .expect("ring gpadl request"),
        "ring gpadl was not accepted"
    );
    gpadl_map.add(
        ring.gpadl_id,
        MultiPagedRangeBuf::from_range_buffer(1, ring.pages).unwrap(),
    );

    // Receive buffer.
    let recv = alloc_gpadl(recv_buf_page_count, &mut next_page, &mut next_gpadl_id);
    let recv_buf_gpadl_id = recv.gpadl_id;
    assert!(
        offer_input
            .request_send
            .call(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: recv.gpadl_id,
                    count: 1,
                    buf: recv.pages.clone(),
                },
            )
            .await
            .expect("recv buf gpadl request"),
        "recv buf gpadl was not accepted"
    );
    gpadl_map.add(
        recv.gpadl_id,
        MultiPagedRangeBuf::from_range_buffer(1, recv.pages).unwrap(),
    );

    // Send buffer.
    let send = alloc_gpadl(send_buf_page_count, &mut next_page, &mut next_gpadl_id);
    let send_buf_gpadl_id = send.gpadl_id;
    assert!(
        offer_input
            .request_send
            .call(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: send.gpadl_id,
                    count: 1,
                    buf: send.pages.clone(),
                },
            )
            .await
            .expect("send buf gpadl request"),
        "send buf gpadl was not accepted"
    );
    gpadl_map.add(
        send.gpadl_id,
        MultiPagedRangeBuf::from_range_buffer(1, send.pages).unwrap(),
    );

    // Snapshot the GPADL ID counter after primary allocations; subchannels
    // will continue from here.
    let subchannel_next_gpadl_id = next_gpadl_id;

    // NOTE: ChannelServerRequest::Restore handling for ALL channels (primary
    // and subchannels) is done by FuzzMockVmbus::add_child which spawns
    // auto-responder tasks.  No per-channel handler created here.

    // Open the channel.
    let host_to_guest_event = Arc::new(SlimEvent::new());
    let host_to_guest_interrupt = {
        let event = host_to_guest_event.clone();
        Interrupt::from_fn(move || event.signal())
    };

    let open_request = OpenRequest {
        open_data: OpenData {
            target_vp: 0,
            ring_offset: 2,
            ring_gpadl_id,
            event_flag: 1,
            connection_id: 1,
            user_data: UserDefinedData::new_zeroed(),
        },
        interrupt: host_to_guest_interrupt,
        use_confidential_ring: false,
        use_confidential_external_memory: false,
    };

    let open_result = offer_input
        .request_send
        .call::<_, _, bool>(ChannelRequest::Open, open_request)
        .await
        .expect("open request");

    assert!(
        open_result,
        "channel open failed unexpectedly in fuzz setup"
    );
    fuzz_vmbus.set_restore_ring_gpadl(ring.gpadl_id);

    let guest_to_host_interrupt = offer_input.event.clone();
    let gpadl_map_view = gpadl_map.view();
    let done = Arc::new(AtomicBool::new(false));
    let raw_channel = gpadl_test_guest_channel(
        &mem,
        &gpadl_map_view,
        ring_gpadl_id,
        2,
        host_to_guest_event,
        guest_to_host_interrupt,
        done,
    );
    let queue = Queue::new(raw_channel).unwrap();

    let subchannel_opener = if max_subchannels > 0 {
        Some(SubchannelOpener {
            pending: subchannel_pending.clone(),
            opened: subchannel_opened.clone(),
            mem: mem.clone(),
            // Subchannel rings start in guest memory immediately after the
            // primary layout (ring + recv_buf + send_buf + data pages).
            next_page: layout.total_pages(),
            next_gpadl_id: subchannel_next_gpadl_id,
        })
    } else {
        None
    };

    let setup = FuzzNicSetup {
        queue,
        mem,
        recv_buf_gpadl_id,
        send_buf_gpadl_id,
        subchannel_opener,
    };

    Ok(NicInternals {
        channel,
        offer_input,
        setup,
        subchannel_opened,
        pending_offers: subchannel_pending,
    })
}

/// Tear down everything created by [`build_nic_internals`]: drop offer inputs
/// to release request senders, then revoke the device handle. The revoke
/// triggers the `Device::run_channel` cleanup which closes any channels that
/// are still open.
///
/// Intentionally avoid sending explicit `ChannelRequest::Close` messages
/// because a prior `restore()` call may have changed the device's view of
/// which channels are open, and closing an already-closed channel would trip
/// an assertion inside `Device::handle_close`.
async fn cleanup_nic_internals(
    offer_input: OfferInput,
    subchannel_opened: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    pending_offers: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    channel: ChannelHandle<Nic>,
) {
    // Drop subchannel offer inputs to release their request senders.
    subchannel_opened.lock().await.clear();

    // Drop any subchannel OfferInputs that were created by
    // `Device::enable_channels` during a `restore()` call but never opened
    // by the fuzz target.  Their `request_send` senders keep the Device's
    // request streams alive, which would hang the revoke wait loop.
    pending_offers.lock().await.clear();

    // Drop the primary offer input so the request stream inside the channel
    // task can drain.
    drop(offer_input);
    let _ = channel.revoke().await;
}

/// A handle returned by [`create_nic_with_channel`] that owns the
/// [`ChannelHandle<Nic>`] alongside the cleanup state.  Call
/// [`NicSetupHandle::cleanup`] when the fuzz loop is done.
pub struct NicSetupHandle {
    /// The VMBus channel device handle. Use this to call `start()`, `stop()`,
    /// `save()`, and `restore()` from within the fuzz loop.
    pub channel: ChannelHandle<Nic>,
    offer_input: OfferInput,
    subchannel_opened: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    pending_offers: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
}

impl NicSetupHandle {
    /// Tear down the NIC: close subchannels, close the primary channel, and
    /// revoke the device handle.  Must be called after the fuzz loop finishes.
    pub async fn cleanup(self) {
        cleanup_nic_internals(
            self.offer_input,
            self.subchannel_opened,
            self.pending_offers,
            self.channel,
        )
        .await;
    }

    /// Send a [`ChannelRequest::Close`] to the device, simulating the host
    /// closing the primary VMBus channel.
    pub fn send_close(&self) {
        self.offer_input
            .request_send
            .send(ChannelRequest::Close(Rpc::detached(())));
    }

    /// Send a [`ChannelRequest::Modify`] with [`ModifyRequest::TargetVp`] to
    /// the device, simulating a VP retarget of the primary channel.
    pub fn send_retarget_vp(&self, target_vp: u32) {
        self.offer_input
            .request_send
            .send(ChannelRequest::Modify(Rpc::detached(
                ModifyRequest::TargetVp { target_vp },
            )));
    }
}

/// Set up a NIC with the specified page layout and custom configuration,
/// returning a ready-to-use queue and GPADL IDs.
pub async fn setup_fuzz_nic_with_config<F, Fut>(
    driver: &pal_async::DefaultDriver,
    layout: &PageLayout,
    config: FuzzNicConfig,
    fuzz_loop: F,
) -> anyhow::Result<()>
where
    F: FnOnce(FuzzNicSetup) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let NicInternals {
        channel,
        offer_input,
        setup,
        subchannel_opened,
        pending_offers,
    } = build_nic_internals(driver, layout, config).await?;

    channel.start();
    let fuzz_result = fuzz_loop(setup).await;
    cleanup_nic_internals(offer_input, subchannel_opened, pending_offers, channel).await;
    fuzz_result
}

/// Like [`setup_fuzz_nic_with_config`] but returns the [`ChannelHandle<Nic>`]
/// alongside the [`FuzzNicSetup`] so that `save()` / `restore()` can be called
/// from the fuzz loop.
///
/// The caller is responsible for calling [`NicSetupHandle::cleanup`] after the
/// fuzz loop finishes and for calling `channel.start()` before sending any
/// VMBus traffic.
pub async fn create_nic_with_channel(
    driver: &pal_async::DefaultDriver,
    layout: &PageLayout,
    config: FuzzNicConfig,
) -> anyhow::Result<(NicSetupHandle, FuzzNicSetup)> {
    let NicInternals {
        channel,
        offer_input,
        setup,
        subchannel_opened,
        pending_offers,
    } = build_nic_internals(driver, layout, config).await?;

    let handle = NicSetupHandle {
        channel,
        offer_input,
        subchannel_opened,
        pending_offers,
    };

    Ok((handle, setup))
}

// ===========================================================================
// SubchannelOpener
// ===========================================================================

/// Drives the opening of VMBus subchannels after the device calls
/// `enable_subchannels`. Call [`SubchannelOpener::open_pending`] after
/// draining the primary queue to let the device process any subchannel
/// requests, then receive guest-side `Queue` handles for each opened
/// subchannel ring.
///
/// Only present in [`FuzzNicSetup`] when `max_subchannels > 0` in
/// [`FuzzNicConfig`].
pub struct SubchannelOpener {
    /// Pending offers pushed by [`MultiChannelMockVmbus::add_child`].
    pending: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    /// Opened offer inputs, shared with setup cleanup.
    opened: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
    mem: GuestMemory,
    /// Next page index in guest memory for subchannel ring allocation.
    next_page: usize,
    /// Next GPADL ID to hand out for subchannel rings.
    next_gpadl_id: u32,
}

impl SubchannelOpener {
    /// Open all subchannels the device has requested since the last call.
    ///
    /// Drains any [`OfferInput`] entries queued by the
    /// [`MultiChannelMockVmbus`], registers GPADL and Open requests on each,
    /// and returns a guest-side `Queue<GpadlRingMem>` per newly opened
    /// subchannel ring. Returns an empty vec if no subchannels are pending.
    pub async fn open_pending(&mut self) -> Vec<Queue<GpadlRingMem>> {
        let new_offers: Vec<OfferInput> = self.pending.lock().await.drain(..).collect();
        let mut queues = Vec::new();
        for offer_input in new_offers {
            let ring_page_start = self.next_page;
            self.next_page += RING_PAGES;
            let gpadl_id = GpadlId(self.next_gpadl_id);
            self.next_gpadl_id += 1;

            // Page list encoding: first element is the byte length of the
            // range, rest are the page-frame numbers (same as primary setup).
            let pages: Vec<u64> = std::iter::once((RING_PAGES * PAGE_SIZE) as u64)
                .chain((ring_page_start..ring_page_start + RING_PAGES).map(|p| p as u64))
                .collect();

            // Register the ring GPADL with this subchannel.
            let accepted = offer_input
                .request_send
                .call(
                    ChannelRequest::Gpadl,
                    GpadlRequest {
                        id: gpadl_id,
                        count: 1,
                        buf: pages.clone(),
                    },
                )
                .await
                .unwrap_or(false);

            if !accepted {
                self.opened.lock().await.push(offer_input);
                continue;
            }

            // Build a dedicated GPADL map for this subchannel's ring.
            let sub_gpadl_map = GpadlMap::new();
            sub_gpadl_map.add(
                gpadl_id,
                MultiPagedRangeBuf::from_range_buffer(1, pages).unwrap(),
            );

            // Open the subchannel.
            let host_to_guest_event = Arc::new(SlimEvent::new());
            let host_to_guest_interrupt = {
                let event = host_to_guest_event.clone();
                Interrupt::from_fn(move || event.signal())
            };
            let open_request = OpenRequest {
                open_data: OpenData {
                    target_vp: 0,
                    ring_offset: 2,
                    ring_gpadl_id: gpadl_id,
                    event_flag: 1,
                    connection_id: 1,
                    user_data: UserDefinedData::new_zeroed(),
                },
                interrupt: host_to_guest_interrupt,
                use_confidential_ring: false,
                use_confidential_external_memory: false,
            };
            let open_result = offer_input
                .request_send
                .call::<_, _, bool>(ChannelRequest::Open, open_request)
                .await
                .unwrap_or(false);

            if open_result {
                let guest_to_host_interrupt = offer_input.event.clone();
                let done = Arc::new(AtomicBool::new(false));
                let raw_channel = gpadl_test_guest_channel(
                    &self.mem,
                    &sub_gpadl_map.view(),
                    gpadl_id,
                    2,
                    host_to_guest_event,
                    guest_to_host_interrupt,
                    done,
                );
                queues.push(Queue::new(raw_channel).unwrap());
            }

            // Retain for cleanup.
            self.opened.lock().await.push(offer_input);
        }
        queues
    }
}
