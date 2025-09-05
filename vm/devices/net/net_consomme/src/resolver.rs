// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConsommeEndpoint;
use consomme::ConsommeParams;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::consomme::ConsommeHandle;
use thiserror::Error;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;

pub struct ConsommeResolver;

declare_static_resolver! {
    ConsommeResolver,
    (NetEndpointHandleKind, ConsommeHandle),
}

#[derive(Debug, Error)]
pub enum ResolveConsommeError {
    #[error(transparent)]
    Consomme(consomme::Error),
    #[error(transparent)]
    InvalidCidr(consomme::InvalidCidr),
}

impl ResolveResource<NetEndpointHandleKind, ConsommeHandle> for ConsommeResolver {
    type Output = ResolvedEndpoint;
    type Error = ResolveConsommeError;

    fn resolve(
        &self,
        resource: ConsommeHandle,
        input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        let mut state = ConsommeParams::new().map_err(ResolveConsommeError::Consomme)?;
        state.client_mac.0 = input.mac_address.to_bytes();
        if let Some(cidr) = &resource.cidr {
            state
                .set_cidr(cidr)
                .map_err(ResolveConsommeError::InvalidCidr)?;
        }
        let endpoint = ConsommeEndpoint::new(state);
        Ok(endpoint.into())
    }
}
