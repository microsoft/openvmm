// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the GET client.

use crate::GuestEmulationTransportClient;
use crate::process_loop::IpmiSelSlot;
use chipset_resources::ipmi_kcs::IpmiSelEventSinkHandleKind;
use chipset_resources::ipmi_kcs::ResolvedIpmiSelEventSink;
use chipset_resources::ipmi_kcs::SelEventSink;
use get_protocol::IPMI_SEL_RECORD_SIZE;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use vm_resource::CanResolveTo;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vm_resource::ResourceKind;

/// A resource kind for getting a [`GuestEmulationTransportClient`].
///
/// This is primarily used with [`PlatformResource`].
pub enum GetClientKind {}

impl ResourceKind for GetClientKind {
    const NAME: &'static str = "get";
}

impl CanResolveTo<GuestEmulationTransportClient> for GetClientKind {
    type Input<'a> = ();
}

impl ResolveResource<GetClientKind, PlatformResource> for GuestEmulationTransportClient {
    type Output = GuestEmulationTransportClient;
    type Error = Infallible;

    fn resolve(
        &self,
        PlatformResource: PlatformResource,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.clone())
    }
}

/// Maximum IPMI SEL notifications accepted but not yet written to the host.
///
/// This bounds memory if the host stops draining the GET pipe; the per-second
/// forwarding limit in the IPMI device bounds only the rate of growth.
const MAX_PENDING_IPMI_SEL_NOTIFICATIONS: usize = 256;

pub(crate) struct GetIpmiSelEventSink {
    client: GuestEmulationTransportClient,
    pending: Arc<AtomicUsize>,
}

impl GetIpmiSelEventSink {
    pub(crate) fn new(client: GuestEmulationTransportClient) -> Self {
        Self {
            client,
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl SelEventSink for GetIpmiSelEventSink {
    fn try_send(&mut self, record_id: u16, record: [u8; IPMI_SEL_RECORD_SIZE]) -> bool {
        let Some(slot) =
            IpmiSelSlot::try_reserve(&self.pending, MAX_PENDING_IPMI_SEL_NOTIFICATIONS)
        else {
            return false;
        };
        self.client.ipmi_sel(record_id, record, slot);
        true
    }
}

/// Resolves the platform IPMI SEL event sink to GET.
pub struct IpmiSelEventSinkResolver(pub GuestEmulationTransportClient);

impl ResolveResource<IpmiSelEventSinkHandleKind, PlatformResource> for IpmiSelEventSinkResolver {
    type Output = ResolvedIpmiSelEventSink;
    type Error = Infallible;

    fn resolve(
        &self,
        PlatformResource: PlatformResource,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedIpmiSelEventSink(Box::new(
            GetIpmiSelEventSink::new(self.0.clone()),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_loop::msg::Msg;

    #[test]
    fn ipmi_sel_sink_bounds_pending_notifications() {
        let (send, mut recv) = mesh::channel::<Msg>();
        let client =
            GuestEmulationTransportClient::new(send, get_protocol::ProtocolVersion::NICKEL_REV2);
        let mut sink = GetIpmiSelEventSink::new(client);
        let record = [0; IPMI_SEL_RECORD_SIZE];

        for record_id in 1..=MAX_PENDING_IPMI_SEL_NOTIFICATIONS as u16 {
            assert!(sink.try_send(record_id, record));
        }
        assert_eq!(sink.pending(), MAX_PENDING_IPMI_SEL_NOTIFICATIONS);
        assert!(!sink.try_send(0x1000, record));

        // Dropping a queued notification releases its reservation.
        drop(recv.try_recv().unwrap());
        assert_eq!(sink.pending(), MAX_PENDING_IPMI_SEL_NOTIFICATIONS - 1);
        assert!(sink.try_send(0x1001, record));
        assert!(!sink.try_send(0x1002, record));
    }
}
