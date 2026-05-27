// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use async_trait::async_trait;
use consomme::ChecksumState;
use consomme::Consomme;
use consomme::ConsommeParams;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use net_backend::BufferAccess;
use net_backend::L4Protocol;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxChecksumState;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

/// Creates and binds a socket for the given protocol, address, and port.
///
/// When `ip_addr` is `None`, binds to `0.0.0.0` (IPv4 only).
pub fn create_bound_socket(
    protocol: &IpProtocol,
    ip_addr: Option<IpAddr>,
    port: u16,
) -> std::io::Result<socket2::Socket> {
    let bind_addr: SocketAddr = match ip_addr {
        Some(IpAddr::V4(ip)) => SocketAddr::V4(SocketAddrV4::new(ip, port)),
        Some(IpAddr::V6(ip)) => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)),
        None => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
    };
    let domain = match bind_addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let (sock_type, sock_protocol) = match protocol {
        IpProtocol::Tcp => (socket2::Type::STREAM, socket2::Protocol::TCP),
        IpProtocol::Udp => (socket2::Type::DGRAM, socket2::Protocol::UDP),
    };
    let socket = socket2::Socket::new(domain, sock_type, Some(sock_protocol))?;
    socket.bind(&bind_addr.into())?;
    Ok(socket)
}

pub struct ConsommeEndpoint {
    endpoint_state: Arc<Mutex<Option<EndpointState>>>,
}

/// Configuration for a port to forward from the host to the guest.
pub struct PortForwardConfig {
    /// The protocol to forward.
    pub protocol: IpProtocol,
    /// An already-bound host socket to forward traffic from.
    pub socket: socket2::Socket,
    /// The port traffic is forwarded to on the guest.
    pub guest_port: u16,
}

struct EndpointState {
    consomme: Consomme,
    recv: Option<mesh::Receiver<ConsommeMessage>>,
    port_forwards: Vec<PortForwardConfig>,
}

impl ConsommeEndpoint {
    pub fn new(state: ConsommeParams) -> Self {
        Self {
            endpoint_state: Arc::new(Mutex::new(Some(EndpointState {
                consomme: Consomme::new(state),
                recv: None,
                port_forwards: Vec::new(),
            }))),
        }
    }

    /// Creates a new endpoint with ports to forward once the queue starts.
    pub fn new_with_ports(state: ConsommeParams, ports: Vec<PortForwardConfig>) -> Self {
        Self {
            endpoint_state: Arc::new(Mutex::new(Some(EndpointState {
                consomme: Consomme::new(state),
                recv: None,
                port_forwards: ports,
            }))),
        }
    }

    pub fn new_dynamic(state: ConsommeParams) -> (Self, ConsommeControl) {
        let consomme = Consomme::new(state);
        let (send, recv) = mesh::channel();
        (
            Self {
                endpoint_state: Arc::new(Mutex::new(Some(EndpointState {
                    consomme,
                    recv: Some(recv),
                    port_forwards: Vec::new(),
                }))),
            },
            ConsommeControl { send },
        )
    }
}

impl InspectMut for ConsommeEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        if let Some(consomme) = &mut *self.endpoint_state.lock() {
            consomme.consomme.inspect_mut(req);
        } else {
            req.respond();
        }
    }
}

/// Provide dynamic updates during runtime.
pub struct ConsommeControl {
    send: mesh::Sender<ConsommeMessage>,
}

/// Error type returned from some dynamic update functions like bind_port.
#[derive(Debug, Error)]
pub enum ConsommeMessageError {
    /// Communication error with running instance.
    #[error("communication error")]
    Mesh(RpcError),
    /// Error executing request on current network instance.
    #[error("bind error")]
    Bind(consomme::BindError),
}

/// Callback to modify network state dynamically.
pub type ConsommeParamsUpdateFn = Box<dyn Fn(&mut ConsommeParams) + Send>;

#[derive(Debug, Clone, Copy)]
pub enum IpProtocol {
    Tcp,
    Udp,
}

/// Configuration for unbinding a previously forwarded port.
struct PortUnbindConfig {
    /// The protocol that was forwarded.
    protocol: IpProtocol,
    /// The guest port that was forwarded.
    guest_port: u16,
}

enum ConsommeMessage {
    BindPort(Rpc<PortForwardConfig, Result<(), consomme::BindError>>),
    UnbindPort(Rpc<PortUnbindConfig, Result<(), consomme::BindError>>),
    UpdateState(Rpc<ConsommeParamsUpdateFn, ()>),
}

impl ConsommeControl {
    /// Binds a port to receive incoming packets.
    pub async fn bind_port(
        &self,
        protocol: IpProtocol,
        ip_addr: Option<IpAddr>,
        port: u16,
    ) -> Result<(), ConsommeMessageError> {
        let socket = create_bound_socket(&protocol, ip_addr, port)
            .map_err(|e| ConsommeMessageError::Bind(consomme::BindError::Io(e)))?;
        let host_addr = socket.local_addr().ok().and_then(|a| a.as_socket());
        tracing::info!(
            ?protocol,
            host_addr = %host_addr.map(|a| a.to_string()).unwrap_or_default(),
            guest_port = %port,
            "port forward socket created"
        );
        self.send
            .call(
                ConsommeMessage::BindPort,
                PortForwardConfig {
                    protocol,
                    socket,
                    guest_port: port,
                },
            )
            .await
            .map_err(ConsommeMessageError::Mesh)?
            .map_err(ConsommeMessageError::Bind)
    }

    /// Unbinds a port previously reserved with bind_port()
    pub async fn unbind_port(
        &self,
        protocol: IpProtocol,
        port: u16,
    ) -> Result<(), ConsommeMessageError> {
        self.send
            .call(
                ConsommeMessage::UnbindPort,
                PortUnbindConfig {
                    protocol,
                    guest_port: port,
                },
            )
            .await
            .map_err(ConsommeMessageError::Mesh)?
            .map_err(ConsommeMessageError::Bind)
    }

    /// Updates dynamic network state
    pub async fn update_state(
        &self,
        f: ConsommeParamsUpdateFn,
    ) -> Result<(), ConsommeMessageError> {
        self.send
            .call(ConsommeMessage::UpdateState, f)
            .await
            .map_err(ConsommeMessageError::Mesh)
    }
}

#[async_trait]
impl net_backend::Endpoint for ConsommeEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "consomme"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.into_iter().next().unwrap();
        let mut queue = Box::new(ConsommeQueue {
            slot: self.endpoint_state.clone(),
            endpoint_state: self.endpoint_state.lock().take(),
            state: QueueState {
                rx_avail: VecDeque::new(),
                rx_ready: VecDeque::new(),
                tx_avail: VecDeque::new(),
                tx_ready: VecDeque::new(),
            },
            stats: Default::default(),
            driver: config.driver,
        });
        let port_forwards =
            std::mem::take(&mut queue.endpoint_state.as_mut().unwrap().port_forwards);
        let bind_result: Result<Vec<_>, _> = queue.with_consomme_no_pool(|c| {
            c.refresh_driver();
            let mut bound: Vec<(IpProtocol, u16)> = Vec::new();
            for fwd in port_forwards {
                let protocol = fwd.protocol;
                let guest_port = fwd.guest_port;
                let result = match protocol {
                    IpProtocol::Tcp => c.bind_tcp_port(fwd.socket, guest_port),
                    IpProtocol::Udp => c.bind_udp_port(fwd.socket, guest_port),
                };
                match result {
                    Ok(()) => bound.push((protocol, guest_port)),
                    Err(err) => {
                        // Roll back successful binds before returning error.
                        for (prev_protocol, prev_guest_port) in &bound {
                            let _ = match prev_protocol {
                                IpProtocol::Tcp => c.unbind_tcp_port(*prev_guest_port),
                                IpProtocol::Udp => c.unbind_udp_port(*prev_guest_port),
                            };
                        }
                        return Err(err);
                    }
                }
            }
            Ok(bound)
        });
        bind_result.map_err(|err| anyhow::anyhow!(err).context("failed to bind port forward"))?;
        queues.push(queue);
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(self.endpoint_state.lock().is_some());
    }

    fn is_ordered(&self) -> bool {
        true
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
            uso: true,
        }
    }
}

pub struct ConsommeQueue {
    slot: Arc<Mutex<Option<EndpointState>>>,
    endpoint_state: Option<EndpointState>,
    state: QueueState,
    stats: Stats,
    driver: Box<dyn Driver>,
}

impl InspectMut for ConsommeQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .merge(&mut self.endpoint_state.as_mut().unwrap().consomme)
            .field("rx_avail", self.state.rx_avail.len())
            .field("rx_ready", self.state.rx_ready.len())
            .field("tx_avail", self.state.tx_avail.len())
            .field("tx_ready", self.state.tx_ready.len())
            .field("stats", &self.stats);
    }
}

impl Drop for ConsommeQueue {
    fn drop(&mut self) {
        *self.slot.lock() = self.endpoint_state.take();
    }
}

impl ConsommeQueue {
    fn with_consomme_no_pool<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut consomme::Access<'_, ClientNoPool<'_>>) -> R,
    {
        f(&mut self
            .endpoint_state
            .as_mut()
            .unwrap()
            .consomme
            .access(&mut ClientNoPool {
                driver: &self.driver,
            }))
    }

    fn with_consomme<F, R>(&mut self, pool: &mut dyn BufferAccess, f: F) -> R
    where
        F: FnOnce(&mut consomme::Access<'_, Client<'_>>) -> R,
    {
        f(&mut self
            .endpoint_state
            .as_mut()
            .unwrap()
            .consomme
            .access(&mut Client {
                state: &mut self.state,
                stats: &mut self.stats,
                driver: &self.driver,
                pool,
            }))
    }

    fn poll_message(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) {
        // process all pending messages
        let state = self.endpoint_state.as_mut().unwrap();
        while let Some(recv) = &mut state.recv {
            match recv.poll_recv(cx) {
                Poll::Ready(Err(err)) => {
                    tracing::warn!(
                        err = &err as &dyn std::error::Error,
                        "Consomme dynamic update channel failure"
                    );
                    state.recv = None;
                    return;
                }
                Poll::Ready(Ok(message)) => process_message(
                    &mut state.consomme.access(&mut Client {
                        state: &mut self.state,
                        stats: &mut self.stats,
                        driver: &self.driver,
                        pool,
                    }),
                    message,
                ),
                Poll::Pending => return,
            }
        }
    }
}

fn process_message(
    consomme: &mut consomme::Access<'_, impl consomme::Client>,
    message: ConsommeMessage,
) {
    match message {
        ConsommeMessage::BindPort(rpc) => {
            rpc.handle_sync(|bind_message| match bind_message.protocol {
                IpProtocol::Tcp => {
                    consomme.bind_tcp_port(bind_message.socket, bind_message.guest_port)
                }
                IpProtocol::Udp => {
                    consomme.bind_udp_port(bind_message.socket, bind_message.guest_port)
                }
            });
        }
        ConsommeMessage::UnbindPort(rpc) => {
            rpc.handle_sync(|unbind_message| match unbind_message.protocol {
                IpProtocol::Tcp => consomme.unbind_tcp_port(unbind_message.guest_port),
                IpProtocol::Udp => consomme.unbind_udp_port(unbind_message.guest_port),
            });
        }
        ConsommeMessage::UpdateState(rpc) => {
            rpc.handle_sync(|f| {
                f(consomme.get_mut().params_mut());
                consomme.update_dns_nameservers()
            });
        }
    }
}

impl net_backend::Queue for ConsommeQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) -> Poll<()> {
        while let Some(head) = self.state.tx_avail.front() {
            let TxSegmentType::Head(meta) = &head.ty else {
                unreachable!()
            };
            let tx_id = meta.id;
            let checksum = ChecksumState {
                ipv4: meta.flags.offload_ip_header_checksum(),
                tcp: meta.flags.offload_tcp_checksum(),
                udp: meta.flags.offload_udp_checksum(),
                tso: meta
                    .flags
                    .offload_tcp_segmentation()
                    .then_some(meta.max_segment_size),
                gso: meta
                    .flags
                    .offload_udp_segmentation()
                    .then_some(meta.max_segment_size),
            };

            let mut buf = vec![0; meta.len as usize];
            let gm = pool.guest_memory();
            let mut offset = 0;
            for segment in self.state.tx_avail.drain(..meta.segment_count as usize) {
                let dest = &mut buf[offset..offset + segment.len as usize];
                if let Err(err) = gm.read_at(segment.gpa, dest) {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "memory write failure"
                    );
                }
                offset += segment.len as usize;
            }

            if let Err(err) = self.with_consomme(pool, |c| c.send(&buf, &checksum)) {
                tracing::debug!(error = &err as &dyn std::error::Error, "tx packet ignored");
                match err {
                    consomme::DropReason::SendBufferFull => self.stats.tx_dropped.increment(),
                    consomme::DropReason::UnsupportedEthertype(_)
                    | consomme::DropReason::UnsupportedIpProtocol(_)
                    | consomme::DropReason::UnsupportedDhcp(_)
                    | consomme::DropReason::UnsupportedArp
                    | consomme::DropReason::UnsupportedDhcpv6(_)
                    | consomme::DropReason::UnsupportedNdp(_) => self.stats.tx_unknown.increment(),
                    consomme::DropReason::Packet(_)
                    | consomme::DropReason::Ipv4Checksum
                    | consomme::DropReason::Io(_)
                    | consomme::DropReason::BadTcpState(_)
                    | consomme::DropReason::FragmentedPacket
                    | consomme::DropReason::IpLengthMismatch
                    | consomme::DropReason::MalformedPacket => self.stats.tx_errors.increment(),
                }
            }

            self.state.tx_ready.push_back(tx_id);
        }

        // TODO: handle messages asynchronously from any queue processing, since
        // there is no guarantee the queue will be processed at all (e.g., if
        // the guest stops processing traffic). This will probably require adding
        // a lock around the consomme state.
        self.poll_message(cx, pool);

        self.with_consomme(pool, |c| c.poll(cx));

        if !self.state.tx_ready.is_empty() || !self.state.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, _pool: &mut dyn BufferAccess, done: &[RxId]) {
        self.state.rx_avail.extend(done);
    }

    fn rx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize> {
        let n = packets.len().min(self.state.rx_ready.len());
        for (x, y) in packets.iter_mut().zip(self.state.rx_ready.drain(..n)) {
            *x = y;
        }
        Ok(n)
    }

    fn tx_avail(
        &mut self,
        _pool: &mut dyn BufferAccess,
        segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        self.state.tx_avail.extend(segments.iter().cloned());
        Ok((false, segments.len()))
    }

    fn tx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        done: &mut [TxId],
    ) -> Result<usize, TxError> {
        let n = done.len().min(self.state.tx_ready.len());
        for (x, y) in done.iter_mut().zip(self.state.tx_ready.drain(..n)) {
            *x = y;
        }
        Ok(n)
    }
}

struct QueueState {
    rx_avail: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
    tx_avail: VecDeque<TxSegment>,
    tx_ready: VecDeque<TxId>,
}

#[derive(Inspect, Default)]
struct Stats {
    rx_dropped: Counter,
    tx_dropped: Counter,
    tx_errors: Counter,
    tx_unknown: Counter,
}

struct Client<'a> {
    state: &'a mut QueueState,
    stats: &'a mut Stats,
    driver: &'a dyn Driver,
    pool: &'a mut dyn BufferAccess,
}

/// Minimal client for consomme operations that don't need BufferAccess
/// (e.g., refresh_driver, timer/socket management).
struct ClientNoPool<'a> {
    driver: &'a dyn Driver,
}

impl consomme::Client for ClientNoPool<'_> {
    fn driver(&self) -> &dyn Driver {
        self.driver
    }

    fn recv(&mut self, _data: &[u8], _checksum: &ChecksumState) {}

    fn rx_mtu(&mut self) -> usize {
        0
    }
}

impl consomme::Client for Client<'_> {
    fn driver(&self) -> &dyn Driver {
        self.driver
    }

    fn recv(&mut self, data: &[u8], checksum: &ChecksumState) {
        let Some(rx_id) = self.state.rx_avail.pop_front() else {
            // This should be rare, only affecting unbuffered protocols. TCP and
            // UDP are buffered and they won't indicate packets unless rx_mtu()
            // returns a non-zero value.
            self.stats.rx_dropped.increment();
            return;
        };
        let max = self.pool.capacity(rx_id) as usize;
        if data.len() <= max {
            self.pool.write_packet(
                rx_id,
                &RxMetadata {
                    offset: 0,
                    len: data.len(),
                    ip_checksum: if checksum.ipv4 {
                        RxChecksumState::Good
                    } else {
                        RxChecksumState::Unknown
                    },
                    l4_checksum: if checksum.tcp || checksum.udp {
                        RxChecksumState::Good
                    } else {
                        RxChecksumState::Unknown
                    },
                    l4_protocol: if checksum.tcp {
                        L4Protocol::Tcp
                    } else if checksum.udp {
                        L4Protocol::Udp
                    } else {
                        L4Protocol::Unknown
                    },
                    vlan: None,
                },
                data,
            );
            self.state.rx_ready.push_back(rx_id);
        } else {
            tracing::warn!(len = data.len(), max, "dropping rx packet: too large");
            self.state.rx_avail.push_front(rx_id);
        }
    }

    fn rx_mtu(&mut self) -> usize {
        if let Some(&rx_id) = self.state.rx_avail.front() {
            self.pool.capacity(rx_id) as usize
        } else {
            0
        }
    }
}
