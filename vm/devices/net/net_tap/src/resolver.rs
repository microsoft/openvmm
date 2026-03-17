// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TapEndpoint;
use crate::tap;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::tap::TapHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;

pub struct TapResolver;

declare_static_resolver! {
    TapResolver,
    (NetEndpointHandleKind, TapHandle),
}

impl ResolveResource<NetEndpointHandleKind, TapHandle> for TapResolver {
    type Output = ResolvedEndpoint;
    type Error = tap::Error;

    fn resolve(
        &self,
        resource: TapHandle,
        _input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        // TODO: accept a pre-opened fd from the resource handle instead of
        // opening by name here, to support fd passing from Kata and similar.
        let fd = tap::open_tap(&resource.name)?;
        let tap = tap::Tap::new(fd)?;

        Ok(TapEndpoint::new(tap)?.into())
    }
}
