// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::DioEndpoint;
use anyhow::Context;
use guid::Guid;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::dio::WindowsDirectIoHandle;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::ResolveResource;

pub struct DioResolver;

declare_static_resolver! {
    DioResolver,
    (NetEndpointHandleKind, WindowsDirectIoHandle),
}

impl ResolveResource<NetEndpointHandleKind, WindowsDirectIoHandle> for DioResolver {
    type Output = ResolvedEndpoint;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: WindowsDirectIoHandle,
        input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        let mut nic =
            vmswitch::dio::DioNic::new(Guid::new_random(), "nic", "nic", input.mac_address.into())
                .context("failed to create a direct I/O NIC")?;

        nic.connect(&vmswitch::kernel::SwitchPortId {
            switch: resource.switch_port_id.switch,
            port: resource.switch_port_id.port,
        })
        .with_context(|| {
            format!(
                "failed to connect port {}:{}",
                resource.switch_port_id.switch, resource.switch_port_id.port
            )
        })?;
        let endpoint = DioEndpoint::new(nic);
        Ok(endpoint.into())
    }
}
