// Copyright (C) Microsoft Corporation. All rights reserved.

//! Resolver-related definitions for networking backends.

use crate::Endpoint;
use net_backend_resources::mac_address::MacAddress;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::CanResolveTo;

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