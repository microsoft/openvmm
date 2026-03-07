// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock VMBus implementations for fuzz targets.
//!
//! [`MultiChannelMockVmbus`] accumulates channel offers into a shared queue,
//! enabling subchannel support. [`FuzzMockVmbus`] wraps it with auto-handling
//! of `ChannelServerRequest::Restore` RPCs so that `save`/`restore` fuzz
//! paths don't hang.

use async_trait::async_trait;
use guestmem::GuestMemory;
use pal_async::task::Spawn;
use std::sync::Arc;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OfferResources;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::bus::RestoreResult;
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::PAGE_SIZE;
use vmcore::interrupt::Interrupt;
use zerocopy::FromZeros;

/// A [`ParentBus`] implementation that supports multiple channels by
/// accumulating every [`OfferInput`] into a shared `Vec`. This allows
/// subchannels to be individually opened after the device calls
/// `enable_subchannels`, unlocking multi-queue code paths.
#[derive(Clone)]
pub(super) struct MultiChannelMockVmbus {
    /// Guest memory shared by all channels.
    pub(super) memory: GuestMemory,
    pending_offers: Arc<futures::lock::Mutex<Vec<OfferInput>>>,
}

impl MultiChannelMockVmbus {
    /// Create a new mock VMBus with `guest_page_count` pages of guest memory.
    pub(super) fn new(guest_page_count: usize) -> Self {
        Self {
            memory: GuestMemory::allocate(guest_page_count * PAGE_SIZE),
            pending_offers: Arc::new(futures::lock::Mutex::new(Vec::new())),
        }
    }

    /// Drain and return all accumulated [`OfferInput`] entries. The first
    /// entry is the primary channel; subsequent entries are subchannels.
    pub(super) async fn drain_pending_offers(&self) -> Vec<OfferInput> {
        self.pending_offers.lock().await.drain(..).collect()
    }

    /// Return a reference-counted handle to the pending-offers queue.
    /// Pass this to a `SubchannelOpener` so it can poll for new subchannel
    /// offers as the device calls `enable_subchannels`.
    pub(super) fn pending_offers_arc(&self) -> Arc<futures::lock::Mutex<Vec<OfferInput>>> {
        self.pending_offers.clone()
    }
}

#[async_trait]
impl ParentBus for MultiChannelMockVmbus {
    async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        self.pending_offers.lock().await.push(request);
        Ok(OfferResources::new(self.memory.clone(), None))
    }

    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }

    fn use_event(&self) -> bool {
        false
    }
}

/// A [`ParentBus`] wrapper around [`MultiChannelMockVmbus`] that automatically
/// spawns a background task to handle [`ChannelServerRequest::Restore`] RPCs
/// for **every** channel (primary + subchannels).  Without this, any call to
/// `ChannelHandle::restore()` would hang because nobody processes the
/// server-side receiver.
#[derive(Clone)]
pub(super) struct FuzzMockVmbus {
    inner: MultiChannelMockVmbus,
    driver: pal_async::DefaultDriver,
    /// Ring GPADL ID for the primary channel, set by `build_nic_internals`
    /// after the channel is successfully opened.  Shared with all clones and
    /// with the spawned server-request handler task.
    ring_gpadl_for_restore: Arc<parking_lot::Mutex<Option<GpadlId>>>,
}

impl FuzzMockVmbus {
    pub(super) fn new(inner: MultiChannelMockVmbus, driver: pal_async::DefaultDriver) -> Self {
        Self {
            inner,
            driver,
            ring_gpadl_for_restore: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Record the ring GPADL ID that was used when opening the primary channel.
    /// Must be called after a successful `ChannelRequest::Open` so that
    /// subsequent `ChannelServerRequest::Restore` responses contain the correct
    /// GPADL ID for `make_rings` to look up.
    pub(super) fn set_restore_ring_gpadl(&self, id: GpadlId) {
        *self.ring_gpadl_for_restore.lock() = Some(id);
    }
}

#[async_trait]
impl ParentBus for FuzzMockVmbus {
    async fn add_child(&self, mut request: OfferInput) -> anyhow::Result<OfferResources> {
        // Extract the server_request_recv and replace with a dummy so the
        // OfferInput stored in pending_offers never gets polled.
        let server_request_recv = std::mem::replace(
            &mut request.server_request_recv,
            mesh::channel::<ChannelServerRequest>().1,
        );
        let ring_gpadl_for_restore = self.ring_gpadl_for_restore.clone();

        // Spawn a detached task that auto-responds to Restore/Revoke RPCs.
        self.driver
            .spawn("fuzz-vmbus-server-request-handler", async move {
                let mut recv = server_request_recv;
                while let Ok(req) = recv.recv().await {
                    match req {
                        ChannelServerRequest::Restore(rpc) => {
                            let open = *rpc.input();
                            let ring_gpadl_id = ring_gpadl_for_restore.lock().unwrap_or(GpadlId(0));
                            let open_request = if open {
                                Some(OpenRequest {
                                    open_data: OpenData {
                                        target_vp: 0,
                                        ring_offset: 2,
                                        ring_gpadl_id,
                                        event_flag: 1,
                                        connection_id: 1,
                                        user_data: UserDefinedData::new_zeroed(),
                                    },
                                    interrupt: Interrupt::from_fn(|| {}),
                                    use_confidential_ring: false,
                                    use_confidential_external_memory: false,
                                })
                            } else {
                                None
                            };
                            rpc.complete(Ok(RestoreResult {
                                open_request,
                                gpadls: vec![],
                            }));
                        }
                        ChannelServerRequest::Revoke(rpc) => rpc.complete(()),
                    }
                }
            })
            .detach();

        self.inner.add_child(request).await
    }

    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }

    fn use_event(&self) -> bool {
        self.inner.use_event()
    }
}
