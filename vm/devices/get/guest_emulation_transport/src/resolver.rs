// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the GET client.

use crate::GuestEmulationTransportClient;
use chipset_resources::ipmi_kcs::IpmiSelEventSinkHandleKind;
use chipset_resources::ipmi_kcs::ResolvedIpmiSelEventSink;
use chipset_resources::ipmi_kcs::SelEventSink;
use chipset_resources::ipmi_kcs::SendOutcome;
use get_protocol::IPMI_SEL_RECORD_SIZE;
use std::convert::Infallible;
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

struct GetIpmiSelEventSink(GuestEmulationTransportClient);

impl SelEventSink for GetIpmiSelEventSink {
    fn try_send(&mut self, record_id: u16, record: [u8; IPMI_SEL_RECORD_SIZE]) -> SendOutcome {
        self.0.ipmi_sel(record_id, record);
        SendOutcome::Accepted
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
        Ok(ResolvedIpmiSelEventSink(Box::new(GetIpmiSelEventSink(
            self.0.clone(),
        ))))
    }
}
