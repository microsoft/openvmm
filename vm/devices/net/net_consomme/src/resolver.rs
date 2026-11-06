// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod addressing;

use crate::ConsommeEndpoint;
use crate::IpProtocol;
use crate::PortForwardConfig;
use crate::create_bound_socket;
use consomme::ConsommeParams;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::consomme::ConsommeHandle;
use net_backend_resources::consomme::HostPort;
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
    #[error(transparent)]
    InvalidStaticIpv4(consomme::static_ipv4::InvalidStaticIpv4),
    #[error(transparent)]
    InvalidStaticIpv6(consomme::static_ipv6::InvalidStaticIpv6),
    #[error("Consomme CIDR and exact static IPv4 configuration are mutually exclusive")]
    ConflictingIpv4Configuration,
    #[error("Consomme exact static IPv6 configuration requires exact static IPv4 configuration")]
    StaticIpv6WithoutStaticIpv4,
    /// The host socket of a port forward cannot be created or bound, as when
    /// another socket holds its port. The message leads with the stable code
    /// `[E_PORT_FORWARD_BIND]`, which survives conversion to a string across
    /// the worker boundary and error wrapping: callers match it, so its
    /// spelling is part of OpenVMM's interface.
    #[error("[E_PORT_FORWARD_BIND] failed to create socket for port forward ({details})")]
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
        addressing::configure(&mut state, &resource)?;
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
                let (bind_port, dynamic_sender) = match p.host_port {
                    HostPort::Fixed(port) => (port, None),
                    HostPort::Dynamic(sender) => (0, Some(sender)),
                };
                let socket = create_bound_socket(&protocol, ip_addr, bind_port).map_err(|e| {
                    ResolveConsommeError::SocketCreation {
                        source: e,
                        details: format!(
                            "{:?} {}:{}",
                            protocol,
                            ip_addr
                                .map(|a| a.to_string())
                                .unwrap_or_else(|| "*".to_string()),
                            bind_port,
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
                if let Some(sender) = dynamic_sender {
                    sender.send(host_addr.expect("just bound an IP socket").port());
                }
                Ok(PortForwardConfig {
                    protocol,
                    socket,
                    guest_port: p.guest_port,
                })
            })
            .collect::<Result<Vec<_>, ResolveConsommeError>>()?;
        let endpoint = if let Some(port_recv) = resource.recv {
            ConsommeEndpoint::new_with_port_channel(state, port_forwards, port_recv)
        } else {
            ConsommeEndpoint::new_with_ports(state, port_forwards)
        };
        let endpoint = match resource.private_network_ingress {
            Some(ingress) => {
                endpoint.with_private_network_ingress(ingress, resource.static_ipv6.is_some())
            }
            None if resource.host_loopback_ingress => {
                endpoint.with_host_loopback_ingress(resource.static_ipv6.is_some())
            }
            None => endpoint,
        };
        Ok(endpoint.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use net_backend_resources::consomme::HostIpAddress;
    use net_backend_resources::consomme::HostPortConfig;
    use net_backend_resources::mac_address::MacAddress;
    use std::net::Ipv4Addr;
    use std::net::TcpListener;
    use test_with_tracing::test;

    #[test]
    fn a_port_forward_that_cannot_bind_leads_with_its_stable_code() {
        // Another socket holds the port that the forward binds.
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let resource = ConsommeHandle {
            cidr: None,
            static_ipv4: None,
            static_ipv6: None,
            ports: vec![HostPortConfig {
                protocol: HostPortProtocol::Tcp,
                host_address: Some(HostIpAddress::Ipv4(Ipv4Addr::LOCALHOST)),
                host_port: HostPort::Fixed(port),
                guest_port: 80,
            }],
            recv: None,
            allow_host_local_access: None,
            map_gateway_to_host_loopback: None,
            gateway_loopback_proxy_port: None,
            private_network_ingress: None,
            host_loopback_ingress: false,
        };
        let input = ResolveEndpointParams {
            mac_address: MacAddress::new([0x52, 0x54, 0, 0, 0, 2]),
        };
        let Err(error) = ConsommeResolver.resolve(resource, input) else {
            panic!("bound port {port}, which another socket holds");
        };
        assert!(
            matches!(error, ResolveConsommeError::SocketCreation { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "[E_PORT_FORWARD_BIND] failed to create socket for port forward \
                 (Tcp 127.0.0.1:{port})"
            )
        );
    }
}
