// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the egress policy of the Consomme endpoint.

use super::*;
use guestmem::GuestMemory;
use net_backend::Endpoint as _;
use net_backend::QueueConfig;
use net_backend::TxMetadata;
use net_backend::TxSegmentType;
use net_backend_resources::egress::EgressPolicyMode;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use std::future::poll_fn;

const GUEST_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const GATEWAY_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];

fn arp_request(target: Ipv4Addr) -> Vec<u8> {
    let mut frame = vec![0u8; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    frame[14..16].copy_from_slice(&1u16.to_be_bytes());
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame[22..28].copy_from_slice(&GUEST_MAC);
    frame[28..32].copy_from_slice(&GUEST_IPV4.octets());
    frame[38..42].copy_from_slice(&target.octets());
    frame
}

fn tx_segment(id: TxId, frame_length: usize) -> TxSegment {
    TxSegment {
        ty: TxSegmentType::Head(TxMetadata {
            id,
            segment_count: 1,
            len: frame_length as u32,
            ..Default::default()
        }),
        gpa: 0,
        len: frame_length as u32,
    }
}

#[pal_async::async_test]
async fn endpoint_policy_proxy_arps_only_bound_on_link_targets(driver: DefaultDriver) {
    let mut params = ConsommeParams::new().unwrap();
    params
        .set_static_ipv4(GUEST_IPV4, 24, GATEWAY_IPV4, GATEWAY_MAC)
        .unwrap();
    params.client_mac.0 = GUEST_MAC;
    let mut endpoint = ConsommeEndpoint::new(params);
    endpoint
        .set_egress_policy(
            EgressPolicy::bind(
                GUEST_IPV4,
                24,
                MacAddress::new(GUEST_MAC),
                GATEWAY_IPV4,
                EgressPolicyMode::TcpEndpoints(vec![
                    "10.0.0.9:443".parse().unwrap(),
                    "192.0.2.7:443".parse().unwrap(),
                ]),
            )
            .unwrap(),
        )
        .unwrap();

    let layout = net_backend::tests::test_layout();
    let memory = GuestMemory::allocate(layout.end_of_ram() as usize);
    let mut pool = net_backend::tests::Bufs::new(memory.clone());
    let mut queues = Vec::new();
    endpoint
        .get_queues(
            vec![QueueConfig {
                driver: Box::new(driver),
            }],
            None,
            &mut queues,
        )
        .await
        .unwrap();
    let queue = &mut queues[0];

    let allowed = arp_request(Ipv4Addr::new(10, 0, 0, 9));
    memory.write_at(0, &allowed).unwrap();
    queue.rx_avail(&mut pool, &[RxId(1)]);
    assert_eq!(
        queue
            .tx_avail(&mut pool, &[tx_segment(TxId(7), allowed.len())])
            .unwrap(),
        (false, 1)
    );
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;

    let mut completed = [TxId(0)];
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 7);
    let mut received = [RxId(0)];
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 1);
    assert_eq!(received[0].0, 1);
    let metadata = pool.rx_metadata(RxId(1)).unwrap();
    assert_eq!(metadata.len, 42);
    let mut reply = vec![0u8; metadata.len];
    memory.read_at(2048, &mut reply).unwrap();
    assert_eq!(&reply[..6], &GUEST_MAC);
    assert_eq!(&reply[6..12], &GATEWAY_MAC);
    assert_eq!(u16::from_be_bytes([reply[20], reply[21]]), 2);
    assert_eq!(&reply[22..28], &GATEWAY_MAC);
    assert_eq!(&reply[28..32], &Ipv4Addr::new(10, 0, 0, 9).octets());
    assert_eq!(&reply[32..38], &GUEST_MAC);
    assert_eq!(&reply[38..42], &GUEST_IPV4.octets());

    let denied = arp_request(Ipv4Addr::new(10, 0, 0, 10));
    memory.write_at(0, &denied).unwrap();
    queue.rx_avail(&mut pool, &[RxId(2)]);
    queue
        .tx_avail(&mut pool, &[tx_segment(TxId(8), denied.len())])
        .unwrap();
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 8);
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 0);

    let mut spoofed = allowed;
    spoofed[11] ^= 1;
    memory.write_at(0, &spoofed).unwrap();
    queue
        .tx_avail(&mut pool, &[tx_segment(TxId(9), spoofed.len())])
        .unwrap();
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 9);
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 0);

    drop(queues);
    endpoint.stop().await;
}

fn set_ipv4_checksum(frame: &mut [u8]) {
    frame[24..26].fill(0);
    let mut sum = frame[14..34].chunks_exact(2).fold(0u32, |sum, word| {
        sum + u32::from(u16::from_be_bytes([word[0], word[1]]))
    });
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    frame[24..26].copy_from_slice(&(!(sum as u16)).to_be_bytes());
}

fn ipv4_frame(destination: Ipv4Addr, protocol: u8, transport: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; 34];
    frame[..6].copy_from_slice(&GATEWAY_MAC);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&((20 + transport.len()) as u16).to_be_bytes());
    frame[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
    frame[22] = 64;
    frame[23] = protocol;
    frame[26..30].copy_from_slice(&GUEST_IPV4.octets());
    frame[30..34].copy_from_slice(&destination.octets());
    frame.extend_from_slice(transport);
    set_ipv4_checksum(&mut frame);
    frame
}

/// A listener announcement of IPv4 TCP `ports`.
fn announcement(ports: &[u16]) -> Vec<u8> {
    use net_backend_resources::ingress::GuestListener;
    use net_backend_resources::ingress::IngressTransport;
    use net_backend_resources::ingress::ListenerAnnouncement;

    let payload = ListenerAnnouncement {
        listeners: ports
            .iter()
            .map(|&port| GuestListener {
                transport: IngressTransport::Tcp,
                port,
                ipv4: true,
                ipv6: false,
            })
            .collect(),
        truncated: false,
    }
    .to_bytes();
    let mut udp = vec![0u8; 8];
    udp[..2].copy_from_slice(&1023u16.to_be_bytes());
    udp[2..4]
        .copy_from_slice(&net_backend_resources::ingress::LISTENER_ANNOUNCEMENT_PORT.to_be_bytes());
    udp[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    udp.extend_from_slice(&payload);
    ipv4_frame(GATEWAY_IPV4, 17, &udp)
}

/// A TCP segment from the guest's `port` to `peer`, whose checksum the guest
/// offloads.
fn tcp_segment(port: u16, peer: SocketAddrV4, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
    let mut tcp = vec![0u8; 20];
    tcp[..2].copy_from_slice(&port.to_be_bytes());
    tcp[2..4].copy_from_slice(&peer.port().to_be_bytes());
    tcp[4..8].copy_from_slice(&seq.to_be_bytes());
    tcp[8..12].copy_from_slice(&ack.to_be_bytes());
    tcp[12] = 5 << 4;
    tcp[13] = flags;
    tcp[14..16].copy_from_slice(&0xffffu16.to_be_bytes());
    ipv4_frame(*peer.ip(), 6, &tcp)
}

struct IngressHarness {
    driver: DefaultDriver,
    memory: GuestMemory,
    pool: net_backend::tests::Bufs,
    queues: Vec<Box<dyn net_backend::Queue>>,
    next_tx: u32,
    next_rx: u32,
}

impl IngressHarness {
    async fn new(driver: DefaultDriver) -> (ConsommeEndpoint, Self) {
        Self::with(
            driver,
            ingress::ListenerPublisher::new(PrivateNetworkIngress { admit_host: true }, false)
                .on_loopback(),
            |policy| policy.with_ingress(net_backend_resources::egress::EgressAction::Allow),
        )
        .await
    }

    /// Starts an endpoint whose `publisher` publishes the guest's ports under
    /// the egress policy that `policy` makes of one that denies everything.
    async fn with(
        driver: DefaultDriver,
        publisher: ingress::ListenerPublisher,
        policy: impl FnOnce(EgressPolicy) -> EgressPolicy,
    ) -> (ConsommeEndpoint, Self) {
        let mut params = ConsommeParams::new().unwrap();
        params
            .set_static_ipv4(GUEST_IPV4, 24, GATEWAY_IPV4, GATEWAY_MAC)
            .unwrap();
        params.client_mac.0 = GUEST_MAC;
        params.allow_host_local_access = true;
        params.map_gateway_to_host_loopback = true;
        let mut endpoint = ConsommeEndpoint::new(params);
        endpoint.endpoint_state.lock().as_mut().unwrap().publisher = Some(publisher);
        endpoint
            .set_egress_policy(policy(
                EgressPolicy::bind(
                    GUEST_IPV4,
                    24,
                    MacAddress::new(GUEST_MAC),
                    GATEWAY_IPV4,
                    EgressPolicyMode::DenyAll,
                )
                .unwrap(),
            ))
            .unwrap();

        let layout = net_backend::tests::test_layout();
        let memory = GuestMemory::allocate(layout.end_of_ram() as usize);
        let pool = net_backend::tests::Bufs::new(memory.clone());
        let mut queues = Vec::new();
        endpoint
            .get_queues(
                vec![QueueConfig {
                    driver: Box::new(driver.clone()),
                }],
                None,
                &mut queues,
            )
            .await
            .unwrap();
        (
            endpoint,
            Self {
                driver,
                memory,
                pool,
                queues,
                next_tx: 1,
                next_rx: 1,
            },
        )
    }

    /// Transmits `frame`, with its TCP checksum offloaded, and returns the
    /// frames that the guest received meanwhile.
    async fn transmit(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        self.memory.write_at(0, frame).unwrap();
        let id = TxId(self.next_tx);
        self.next_tx += 1;
        let segment = TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id,
                segment_count: 1,
                len: frame.len() as u32,
                flags: net_backend::TxFlags::new().with_offload_tcp_checksum(true),
                ..Default::default()
            }),
            gpa: 0,
            len: frame.len() as u32,
        };
        let queue = &mut self.queues[0];
        queue.tx_avail(&mut self.pool, &[segment]).unwrap();
        poll_fn(|cx| queue.poll_ready(cx, &mut self.pool)).await;
        let mut completed = [TxId(0)];
        assert_eq!(queue.tx_poll(&mut self.pool, &mut completed).unwrap(), 1);
        assert_eq!(completed[0].0, id.0);
        self.received()
    }

    /// Returns the frames that the guest received.
    fn received(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut ids = [RxId(0); 8];
        let count = self.queues[0].rx_poll(&mut self.pool, &mut ids).unwrap();
        for id in &ids[..count] {
            let metadata = self.pool.rx_metadata(*id).unwrap();
            let mut frame = vec![0u8; metadata.len];
            self.memory.read_at(id.0 as u64 * 2048, &mut frame).unwrap();
            frames.push(frame);
        }
        frames
    }

    /// Polls until the guest receives a TCP segment for `port` that `check`
    /// accepts, and returns it.
    async fn receive_tcp(&mut self, port: u16, check: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if self.next_rx < 16 {
                let ids = (self.next_rx..16).map(RxId).collect::<Vec<_>>();
                self.queues[0].rx_avail(&mut self.pool, &ids);
                self.next_rx = 16;
            }
            let queue = &mut self.queues[0];
            poll_fn(|cx| {
                let _ = queue.poll_ready(cx, &mut self.pool);
                Poll::Ready(())
            })
            .await;
            if let Some(frame) = self.received().into_iter().find(|frame| {
                frame.len() >= 54
                    && frame[23] == 6
                    && u16::from_be_bytes([frame[36], frame[37]]) == port
                    && check(frame)
            }) {
                return frame;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for a TCP segment to guest port {port}"
            );
            pal_async::timer::PolledTimer::new(&self.driver)
                .sleep(std::time::Duration::from_millis(10))
                .await;
        }
    }
}

/// Test that the endpoint publishes the ports that the guest announces,
/// admits the guest's replies to the connections that they accept under an
/// egress posture that denies everything else, and stops publishing a port
/// that the guest withdraws.
#[pal_async::async_test]
async fn endpoint_publishes_announced_ports_and_admits_their_replies(driver: DefaultDriver) {
    const SYN: u8 = 0x02;
    const ACK: u8 = 0x10;
    let port = {
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let (mut endpoint, mut harness) = IngressHarness::new(driver).await;

    // The endpoint consumes the announcement instead of sending it.
    assert!(harness.transmit(&announcement(&[port])).await.is_empty());

    // A host client reaches the guest port through the published listener.
    let mut client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let syn = harness
        .receive_tcp(port, |frame| frame[47] & (SYN | ACK) == SYN)
        .await;
    let peer = SocketAddrV4::new(
        Ipv4Addr::new(syn[26], syn[27], syn[28], syn[29]),
        u16::from_be_bytes([syn[34], syn[35]]),
    );
    let peer_seq = u32::from_be_bytes([syn[38], syn[39], syn[40], syn[41]]);

    // An uncarried reply is dropped, so Consomme does not even reset it.
    let stray = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 5000);
    assert!(
        harness
            .transmit(&tcp_segment(port, stray, 1000, 1, SYN | ACK))
            .await
            .is_empty()
    );
    // The guest cannot open connections of its own.
    assert!(
        harness
            .transmit(&tcp_segment(port, stray, 1000, 0, SYN))
            .await
            .is_empty()
    );

    // The guest's reply to the accepted connection passes, after which the
    // client's data reaches the guest.
    harness
        .transmit(&tcp_segment(
            port,
            peer,
            1000,
            peer_seq.wrapping_add(1),
            SYN | ACK,
        ))
        .await;
    std::io::Write::write_all(&mut client, b"hello").unwrap();
    harness
        .receive_tcp(port, |frame| frame.ends_with(b"hello"))
        .await;

    // Withdrawing the port closes its listener.
    harness.transmit(&announcement(&[])).await;
    assert!(
        std::net::TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            std::time::Duration::from_secs(5)
        )
        .is_err()
    );

    drop(harness);
    let published = endpoint
        .endpoint_state
        .lock()
        .as_ref()
        .unwrap()
        .publisher
        .as_ref()
        .unwrap()
        .published();
    assert!(published.is_empty());
    endpoint.stop().await;
}

/// Test that host-loopback ingress alone publishes the announced ports on
/// host loopback, for the host, and admits the guest's replies under an
/// egress posture that denies everything else.
#[pal_async::async_test]
async fn endpoint_publishes_announced_ports_on_host_loopback(driver: DefaultDriver) {
    const SYN: u8 = 0x02;
    const ACK: u8 = 0x10;
    let port = {
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let (mut endpoint, mut harness) = IngressHarness::with(
        driver,
        ingress::ListenerPublisher::host_loopback(false),
        |policy| {
            policy.with_host_loopback_ingress(net_backend_resources::egress::EgressAction::Allow)
        },
    )
    .await;

    assert!(harness.transmit(&announcement(&[port])).await.is_empty());
    let mut client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let syn = harness
        .receive_tcp(port, |frame| frame[47] & (SYN | ACK) == SYN)
        .await;
    let peer = SocketAddrV4::new(
        Ipv4Addr::new(syn[26], syn[27], syn[28], syn[29]),
        u16::from_be_bytes([syn[34], syn[35]]),
    );
    let peer_seq = u32::from_be_bytes([syn[38], syn[39], syn[40], syn[41]]);
    harness
        .transmit(&tcp_segment(
            port,
            peer,
            1000,
            peer_seq.wrapping_add(1),
            SYN | ACK,
        ))
        .await;
    std::io::Write::write_all(&mut client, b"hello").unwrap();
    harness
        .receive_tcp(port, |frame| frame.ends_with(b"hello"))
        .await;

    drop(harness);
    let published = endpoint
        .endpoint_state
        .lock()
        .as_ref()
        .unwrap()
        .publisher
        .as_ref()
        .unwrap()
        .published();
    assert_eq!(
        published,
        [(
            net_backend_resources::ingress::IngressTransport::Tcp,
            false,
            port
        )]
    );
    endpoint.stop().await;
}

/// Test that the gateway's rules govern the guest's own connections to the
/// virtual address at which the endpoint presents a host-loopback peer, so
/// that the address reaches no host-loopback port that they deny.
#[pal_async::async_test]
async fn endpoint_applies_the_gateways_rules_to_host_loopback_peers(driver: DefaultDriver) {
    use net_backend_resources::egress::EgressAction;
    const SYN: u8 = 0x02;
    const ACK: u8 = 0x10;
    let port = {
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let denied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let allowed = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    denied.set_nonblocking(true).unwrap();
    allowed.set_nonblocking(true).unwrap();
    let denied_port = denied.local_addr().unwrap().port();
    let allowed_port = allowed.local_addr().unwrap().port();
    let (mut endpoint, mut harness) = IngressHarness::with(
        driver,
        ingress::ListenerPublisher::host_loopback(false),
        |_| {
            EgressPolicy::bind(
                GUEST_IPV4,
                24,
                MacAddress::new(GUEST_MAC),
                GATEWAY_IPV4,
                EgressPolicyMode::Rules {
                    default_action: EgressAction::Allow,
                    allow: Vec::new(),
                    deny: vec![
                        format!("{GATEWAY_IPV4}/32:tcp:{denied_port}")
                            .parse()
                            .unwrap(),
                    ],
                },
            )
            .unwrap()
            .with_host_loopback_ingress(EgressAction::Allow)
        },
    )
    .await;

    // A host client of the published port shows the guest the address at
    // which the endpoint presents host loopback.
    assert!(harness.transmit(&announcement(&[port])).await.is_empty());
    let _client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let syn = harness
        .receive_tcp(port, |frame| frame[47] & (SYN | ACK) == SYN)
        .await;
    let peer = Ipv4Addr::new(syn[26], syn[27], syn[28], syn[29]);
    assert_ne!(peer, GATEWAY_IPV4);

    // Neither the gateway nor that address reaches the port that the rules
    // deny at the gateway, while the address reaches another one.
    for (guest_port, destination) in [(40000, GATEWAY_IPV4), (40001, peer)] {
        harness
            .transmit(&tcp_segment(
                guest_port,
                SocketAddrV4::new(destination, denied_port),
                1000,
                0,
                SYN,
            ))
            .await;
    }
    harness
        .transmit(&tcp_segment(
            40002,
            SocketAddrV4::new(peer, allowed_port),
            1000,
            0,
            SYN,
        ))
        .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match allowed.accept() {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for the allowed connection"
                );
                pal_async::timer::PolledTimer::new(&harness.driver)
                    .sleep(std::time::Duration::from_millis(10))
                    .await;
            }
            Err(error) => panic!("accepting the allowed connection failed: {error}"),
        }
    }
    // Consomme opens host connections in the order that the guest asks for
    // them, so a denied one would have reached its listener first.
    assert_eq!(
        denied.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    drop(harness);
    endpoint.stop().await;
}
