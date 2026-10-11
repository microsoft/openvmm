// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publication of the guest's listening ports on the host.
//!
//! With private-network or host-loopback ingress, the guest announces the TCP
//! and UDP ports on which it listens (see [`net_backend_resources::ingress`]).
//! The endpoint consumes each announcement and keeps one host listener per
//! announced port and address family, at the guest's port number. For
//! private-network ingress, the listener is bound to every host address and
//! admits only the peers that [`consomme::InboundAdmission::PrivateNetwork`]
//! admits. For host-loopback ingress alone, it is bound to the family's
//! loopback address and admits only the host, as
//! [`consomme::InboundAdmission::Host`] does, and it never displaces a host
//! listener that holds the port on every host address. A host listener that
//! binds every host address later fails on Linux, while on Windows, unless it
//! asks for exclusive use of the port, it shares the port and the publication
//! keeps its loopback traffic, as any host process's loopback listener would.
//! Ports that the guest no longer announces lose their listeners, and
//! listeners that the host cannot bind, for example because a host process
//! uses the port, are retried with the next announcement.

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

    /// Returns the guest's port on every host address of the family or, with
    /// `loopback`, on the family's loopback address.
    fn address(self, loopback: bool) -> SocketAddr {
        match (self.ipv6, loopback) {
            (false, false) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.port)),
            (false, true) => SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            (true, false) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, self.port)),
            (true, true) => SocketAddr::from((Ipv6Addr::LOCALHOST, self.port)),
        }
    }

    /// Creates a host socket of the publication's transport bound to
    /// `address`. An IPv6 socket serves IPv4 as well with `dual_stack`.
    fn socket_at(self, address: SocketAddr, dual_stack: bool) -> std::io::Result<socket2::Socket> {
        let domain = if address.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let (kind, protocol) = match self.transport {
            IngressTransport::Tcp => (socket2::Type::STREAM, socket2::Protocol::TCP),
            IngressTransport::Udp => (socket2::Type::DGRAM, socket2::Protocol::UDP),
        };
        let socket = socket2::Socket::new(domain, kind, Some(protocol))?;
        if address.is_ipv6() {
            socket.set_only_v6(!dual_stack)?;
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

    /// Creates the host socket, bound to every host address of the family at
    /// the guest's port, or with `loopback`, to the family's loopback address.
    fn bind_socket(self, loopback: bool) -> std::io::Result<socket2::Socket> {
        // A socket bound to loopback alone can take the port's loopback
        // traffic from a host listener bound to every host address: Windows
        // permits that between sockets of one user, and BSD-derived systems
        // with address reuse. Binding every host address first fails while
        // such a listener holds the port, which leaves the port with it. Linux
        // refuses the loopback binding itself.
        //
        // Windows binds every address of one family and, for a dual-stack
        // socket, every address of both families independently, so a
        // dual-stack probe finds the dual-stack listeners. It lets the
        // loopback binding share the port with both probes, which therefore
        // hold those addresses until the loopback binding holds the port, so
        // that no host listener can bind them in between.
        #[cfg(windows)]
        let _probes = if loopback {
            Some((
                self.socket_at(self.address(false), false)?,
                self.socket_at(SocketAddr::from((Ipv6Addr::UNSPECIFIED, self.port)), true)?,
            ))
        } else {
            None
        };
        #[cfg(all(unix, not(target_os = "linux")))]
        if loopback {
            drop(self.socket_at(self.address(false), false)?);
        }
        self.socket_at(self.address(loopback), false)
    }
}

/// The host listeners that publish the guest's announced ports.
pub(crate) struct ListenerPublisher {
    admission: InboundAdmission,
    guest_has_ipv6: bool,
    bound: BTreeSet<Publication>,
    /// Binds the listeners to loopback: for host-loopback ingress, and in
    /// tests of private-network ingress so as not to listen on the host's
    /// networks.
    loopback: bool,
    /// Where the listeners publish the guest's ports, for diagnostics.
    scope: &'static str,
}

impl ListenerPublisher {
    /// Publishes the guest's ports to the host's private networks.
    pub(crate) fn new(ingress: PrivateNetworkIngress, guest_has_ipv6: bool) -> Self {
        Self {
            admission: InboundAdmission::PrivateNetwork {
                host: ingress.admit_host,
            },
            guest_has_ipv6,
            bound: BTreeSet::new(),
            loopback: false,
            scope: "private networks",
        }
    }

    /// Publishes the guest's ports on host loopback alone, for the host.
    pub(crate) fn host_loopback(guest_has_ipv6: bool) -> Self {
        Self {
            admission: InboundAdmission::Host,
            guest_has_ipv6,
            bound: BTreeSet::new(),
            loopback: true,
            scope: "host loopback",
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
                    scope = self.scope,
                    transport = ?publication.transport,
                    ipv6 = publication.ipv6,
                    port = publication.port,
                    "stopped publishing a guest port"
                ),
                Err(error) => tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    scope = self.scope,
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
                        scope = self.scope,
                        transport = ?publication.transport,
                        ipv6 = publication.ipv6,
                        port = publication.port,
                        "publishing a guest port"
                    );
                    self.bound.insert(publication);
                }
                Err(error) => tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    scope = self.scope,
                    transport = ?publication.transport,
                    ipv6 = publication.ipv6,
                    port = publication.port,
                    "failed to publish a guest port"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(transport: IngressTransport, port: u16) -> Publication {
        Publication {
            transport,
            ipv6: false,
            port,
        }
    }

    fn tcp(port: u16) -> Publication {
        publication(IngressTransport::Tcp, port)
    }

    /// Binds a TCP socket without address reuse to `address`, as a host
    /// process that holds the port would, without listening on it.
    fn hold(address: SocketAddr) -> socket2::Socket {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .unwrap();
        socket.bind(&address.into()).unwrap();
        socket
    }

    fn port_of(socket: &socket2::Socket) -> u16 {
        socket.local_addr().unwrap().as_socket().unwrap().port()
    }

    #[test]
    fn host_loopback_publications_bind_loopback_alone() {
        for transport in [IngressTransport::Tcp, IngressTransport::Udp] {
            // Another process may take a port that was just free, such as for
            // a loopback client, so another free port is tried then.
            let (port, socket) = (0..10)
                .find_map(|_| {
                    let port = port_of(&hold(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))));
                    match publication(transport, port).bind_socket(true) {
                        Ok(socket) => Some((port, socket)),
                        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => None,
                        Err(error) => panic!("{transport:?}: {error}"),
                    }
                })
                .unwrap_or_else(|| panic!("{transport:?}: no free port"));
            assert_eq!(
                socket.local_addr().unwrap().as_socket(),
                Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
                "{transport:?}"
            );
        }
    }

    #[test]
    fn host_loopback_publications_never_displace_host_listeners() {
        for address in [Ipv4Addr::UNSPECIFIED, Ipv4Addr::LOCALHOST] {
            let held = hold(SocketAddr::from((address, 0)));
            assert!(tcp(port_of(&held)).bind_socket(true).is_err(), "{address}");
        }
    }

    #[test]
    fn host_loopback_publications_never_displace_dual_stack_listeners() {
        for transport in [IngressTransport::Tcp, IngressTransport::Udp] {
            let (kind, protocol) = match transport {
                IngressTransport::Tcp => (socket2::Type::STREAM, socket2::Protocol::TCP),
                IngressTransport::Udp => (socket2::Type::DGRAM, socket2::Protocol::UDP),
            };
            let held = socket2::Socket::new(socket2::Domain::IPV6, kind, Some(protocol)).unwrap();
            held.set_only_v6(false).unwrap();
            match held.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into()) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                    ) =>
                {
                    return;
                }
                Err(error) => panic!("dual-stack bind failed: {error}"),
            }
            if transport == IngressTransport::Tcp {
                held.listen(1).unwrap();
            }
            let port = port_of(&held);
            for ipv6 in [false, true] {
                let publication = Publication {
                    transport,
                    ipv6,
                    port,
                };
                assert!(
                    publication.bind_socket(true).is_err(),
                    "{transport:?} ipv6={ipv6}"
                );
            }
        }
    }
}
