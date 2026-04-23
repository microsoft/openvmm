// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConsommeEndpoint;
use crate::PortForwardConfig;
use consomme::ConsommeParams;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::consomme::ConsommeHandle;
use net_backend_resources::consomme::HostPortProtocol;
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
    #[error("invalid host forward address '{0}'")]
    InvalidAddress(String),
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
        let port_forwards: Vec<PortForwardConfig> = resource
            .ports
            .into_iter()
            .map(|p| {
                let address = p
                    .host_address
                    .as_deref()
                    .map(|a| {
                        a.parse()
                            .map_err(|_| ResolveConsommeError::InvalidAddress(a.to_owned()))
                    })
                    .transpose()?;
                Ok(PortForwardConfig {
                    protocol: match p.protocol {
                        HostPortProtocol::Tcp => crate::IpProtocol::Tcp,
                        HostPortProtocol::Udp => crate::IpProtocol::Udp,
                    },
                    address,
                    host_port: p.host_port,
                    guest_port: p.guest_port,
                })
            })
            .collect::<Result<_, ResolveConsommeError>>()?;
        let endpoint = ConsommeEndpoint::new_with_ports(state, port_forwards);
        Ok(endpoint.into())
    }
}
