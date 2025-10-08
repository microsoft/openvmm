// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver-related definitions for networking backends.

use crate::Endpoint;
use net_backend_resources::mac_address::MacAddress;
use vm_resource::CanResolveTo;
use vm_resource::kind::NetEndpointHandleKind;

pub struct ResolveEndpointParams {
    pub mac_address: MacAddress,
}

impl CanResolveTo<ResolvedEndpoint> for NetEndpointHandleKind {
    type Input<'a> = ResolveEndpointParams;
}

pub struct ResolvedEndpoint(pub Box<dyn Endpoint>);

impl<T: 'static + Endpoint> From<T> for ResolvedEndpoint {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}
