// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Nic;
use async_trait::async_trait;
use net_backend::resolve::ResolveEndpointParams;
use netvsp_resources::NetvspHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;

pub struct NetvspResolver;

declare_static_async_resolver! {
    NetvspResolver,
    (VmbusDeviceHandleKind, NetvspHandle),
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, NetvspHandle> for NetvspResolver {
    type Output = ResolvedVmbusDevice;
    type Error = ResolveError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: NetvspHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let endpoint = resolver
            .resolve(
                resource.endpoint,
                ResolveEndpointParams {
                    mac_address: resource.mac_address,
                },
            )
            .await?;

        let mut builder = Nic::builder();
        if let Some(max_queues) = resource.max_queues {
            builder = builder.max_queues(max_queues);
        }
        let nic = builder.build(
            input.driver_source,
            resource.instance_id,
            endpoint.0,
            resource.mac_address,
            resource.instance_id.data1,
        );
        Ok(nic.into())
    }
}
