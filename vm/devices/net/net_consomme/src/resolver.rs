// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConsommeEndpoint;
use crate::IpProtocol;
use crate::PortForwardConfig;
use crate::create_bound_socket;
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
    #[error("failed to create socket for port forward ({details})")]
    SocketCreation {
        #[source]
        source: std::io::Error,
        details: String,
    },
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
                let protocol = match p.protocol {
                    HostPortProtocol::Tcp => IpProtocol::Tcp,
                    HostPortProtocol::Udp => IpProtocol::Udp,
                };
                let ip_addr = p.host_address.map(std::net::IpAddr::from);
                let socket = create_bound_socket(&protocol, ip_addr, p.host_port).map_err(|e| {
                    ResolveConsommeError::SocketCreation {
                        source: e,
                        details: format!(
                            "{:?} {}:{}",
                            protocol,
                            ip_addr
                                .map(|a| a.to_string())
                                .unwrap_or_else(|| "*".to_string()),
                            p.host_port,
                        ),
                    }
                })?;
                let host_addr = socket.local_addr().ok().and_then(|a| a.as_socket());
                tracing::info!(
                    ?protocol,
                    host_addr = %host_addr.map(|a| a.to_string()).unwrap_or_default(),
                    guest_port = %p.guest_port,
                    "port forward socket created"
                );
                Ok(PortForwardConfig {
                    protocol,
                    socket,
                    guest_port: p.guest_port,
                })
            })
            .collect::<Result<Vec<_>, ResolveConsommeError>>()?;
        let endpoint = ConsommeEndpoint::new_with_ports(state, port_forwards);
        Ok(endpoint.into())
    }
}
