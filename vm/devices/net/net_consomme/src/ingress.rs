// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publication of the guest's listening ports to the host's private networks.
//!
//! With private-network ingress, the guest announces the TCP and UDP ports on
//! which it listens (see [`net_backend_resources::ingress`]). The endpoint
//! consumes each announcement and keeps one host listener per announced port
//! and address family: bound to every host address at the guest's port
//! number, and admitting only the peers that
//! [`consomme::InboundAdmission::PrivateNetwork`] admits. Ports that the guest
//! no longer announces lose their listeners, and listeners that the host
//! cannot bind, for example because a host process uses the port, are retried
//! with the next announcement.

use consomme::InboundAdmission;
use consomme::IpVersion;
use net_backend_resources::consomme::PrivateNetworkIngress;
use net_backend_resources::ingress::IngressTransport;
use net_backend_resources::ingress::ListenerAnnouncement;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

/// One host listener that publishes a guest port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Publication {
    transport: IngressTransport,
    ipv6: bool,
    port: u16,
}

impl Publication {
    fn family(self) -> IpVersion {
        if self.ipv6 {
            IpVersion::Ipv6
        } else {
            IpVersion::Ipv4
        }
    }

    /// Creates the host socket, bound to every host address of the family at
    /// the guest's port, or with `loopback`, to the family's loopback address.
    fn bind_socket(self, loopback: bool) -> std::io::Result<socket2::Socket> {
        let (domain, address) = if self.ipv6 {
            (
                socket2::Domain::IPV6,
                SocketAddr::from((
                    if loopback {
                        Ipv6Addr::LOCALHOST
                    } else {
                        Ipv6Addr::UNSPECIFIED
                    },
                    self.port,
                )),
            )
        } else {
            (
                socket2::Domain::IPV4,
                SocketAddr::from((
                    if loopback {
                        Ipv4Addr::LOCALHOST
                    } else {
                        Ipv4Addr::UNSPECIFIED
                    },
                    self.port,
                )),
            )
        };
        let (kind, protocol) = match self.transport {
            IngressTransport::Tcp => (socket2::Type::STREAM, socket2::Protocol::TCP),
            IngressTransport::Udp => (socket2::Type::DGRAM, socket2::Protocol::UDP),
        };
        let socket = socket2::Socket::new(domain, kind, Some(protocol))?;
        if self.ipv6 {
            socket.set_only_v6(true)?;
        }
        // Lets a listener that the guest closes and reopens bind again while
        // its earlier connections linger in TIME_WAIT. Windows allows that by
        // default, and its SO_REUSEADDR would let other sockets share the port.
        #[cfg(unix)]
        if self.transport == IngressTransport::Tcp {
            socket.set_reuse_address(true)?;
        }
        socket.bind(&address.into())?;
        Ok(socket)
    }
}

/// The host listeners that publish the guest's announced ports.
pub(crate) struct PrivateNetworkPublisher {
    admission: InboundAdmission,
    guest_has_ipv6: bool,
    bound: BTreeSet<Publication>,
    /// Binds the listeners to loopback, which tests use so as not to listen
    /// on the host's networks.
    loopback: bool,
}

impl PrivateNetworkPublisher {
    pub(crate) fn new(ingress: PrivateNetworkIngress, guest_has_ipv6: bool) -> Self {
        Self {
            admission: InboundAdmission::PrivateNetwork {
                host: ingress.admit_host,
            },
            guest_has_ipv6,
            bound: BTreeSet::new(),
            loopback: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn on_loopback(mut self) -> Self {
        self.loopback = true;
        self
    }

    /// Returns the publications that `announcement` asks for. IPv6 listeners
    /// need the guest's IPv6 identity.
    fn wanted(&self, announcement: &ListenerAnnouncement) -> BTreeSet<Publication> {
        announcement
            .listeners
            .iter()
            .flat_map(|listener| {
                [
                    listener.ipv4.then_some(false),
                    (listener.ipv6 && self.guest_has_ipv6).then_some(true),
                ]
                .into_iter()
                .flatten()
                .map(|ipv6| Publication {
                    transport: listener.transport,
                    ipv6,
                    port: listener.port,
                })
            })
            .collect()
    }

    /// Applies an announcement's UDP payload: unbinds the listeners of ports
    /// that the guest no longer announces, and binds listeners for the
    /// announced ports that have none.
    pub(crate) fn apply(
        &mut self,
        payload: &[u8],
        consomme: &mut consomme::Access<'_, impl consomme::Client>,
    ) {
        let announcement = match ListenerAnnouncement::parse(payload) {
            Ok(announcement) => announcement,
            Err(error) => {
                tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "ignoring an invalid guest listener announcement"
                );
                return;
            }
        };
        if announcement.truncated {
            tracelimit::warn_ratelimited!(
                "the guest listens on more ports than one announcement carries; only the lowest are published"
            );
        }
        let wanted = self.wanted(&announcement);
        let withdrawn = self.bound.difference(&wanted).copied().collect::<Vec<_>>();
        for publication in withdrawn {
            self.bound.remove(&publication);
            let result = match publication.transport {
                IngressTransport::Tcp => {
                    consomme.unbind_published_tcp_port(publication.family(), publication.port)
                }
                IngressTransport::Udp => {
                    consomme.unbind_published_udp_port(publication.family(), publication.port)
                }
            };
            match result {
                Ok(()) => tracing::info!(
                    transport = ?publication.transport,
                    ipv6 = publication.ipv6,
                    port = publication.port,
                    "stopped publishing a guest port to private networks"
                ),
                Err(error) => tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    transport = ?publication.transport,
                    ipv6 = publication.ipv6,
                    port = publication.port,
                    "failed to stop publishing a guest port"
                ),
            }
        }
        let added = wanted.difference(&self.bound).copied().collect::<Vec<_>>();
        for publication in added {
            let result = publication
                .bind_socket(self.loopback)
                .map_err(consomme::BindError::Io)
                .and_then(|socket| match publication.transport {
                    IngressTransport::Tcp => consomme.bind_tcp_port_with_admission(
                        socket,
                        publication.port,
                        self.admission,
                    ),
                    IngressTransport::Udp => consomme.bind_udp_port_with_admission(
                        socket,
                        publication.port,
                        self.admission,
                    ),
                });
            match result {
                Ok(()) => {
                    tracing::info!(
                        transport = ?publication.transport,
                        ipv6 = publication.ipv6,
                        port = publication.port,
                        "publishing a guest port to private networks"
                    );
                    self.bound.insert(publication);
                }
                Err(error) => tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    transport = ?publication.transport,
                    ipv6 = publication.ipv6,
                    port = publication.port,
                    "failed to publish a guest port to private networks"
                ),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn published(&self) -> Vec<(IngressTransport, bool, u16)> {
        self.bound
            .iter()
            .map(|publication| (publication.transport, publication.ipv6, publication.port))
            .collect()
    }
}
