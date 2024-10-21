// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConsommeEndpoint;
use consomme::ConsommeState;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::consomme::ConsommeHandle;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::ResolveResource;

pub struct ConsommeResolver;

declare_static_resolver! {
    ConsommeResolver,
    (NetEndpointHandleKind, ConsommeHandle),
}

impl ResolveResource<NetEndpointHandleKind, ConsommeHandle> for ConsommeResolver {
    type Output = ResolvedEndpoint;
    type Error = consomme::Error;

    fn resolve(
        &self,
        ConsommeHandle: ConsommeHandle,
        input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        let mut state = ConsommeState::new()?;
        state.client_mac.0 = input.mac_address.to_bytes();
        let endpoint = ConsommeEndpoint::new_with_state(state);
        Ok(endpoint.into())
    }
}
