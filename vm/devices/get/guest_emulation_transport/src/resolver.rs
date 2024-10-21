// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the GET client.

use crate::GuestEmulationTransportClient;
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
