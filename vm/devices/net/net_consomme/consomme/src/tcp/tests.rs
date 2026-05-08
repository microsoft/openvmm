// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::BindError;
use crate::ChecksumState;
use crate::Client;
use crate::Consomme;
use crate::ConsommeParams;
use futures::AsyncRead;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use parking_lot::Mutex;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv4Repr;
use std::net::Ipv4Addr;
use std::net::SocketAddrV4;
use std::sync::Arc;

// ── Mock client ────────────────────────────────────────────────────

struct TestClient {
    driver: DefaultDriver,
    received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl TestClient {
    fn new(driver: DefaultDriver) -> Self {
        Self {
            driver,
            received_packets: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Client for TestClient {
    fn driver(&self) -> &dyn Driver {
        &self.driver
    }

    fn recv(&mut self, data: &[u8], _checksum: &ChecksumState) {
        self.received_packets.lock().push(data.to_vec());
    }

    fn rx_mtu(&mut self) -> usize {
        1514
    }
}

// ── Packet helpers ─────────────────────────────────────────────────

/// Build a TCP packet inside an Ethernet/IPv4 frame.
/// Returns the total frame length.
fn build_tcp_packet(
    buf: &mut [u8],
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src_ip: Ipv4Address,
    dst_ip: Ipv4Address,
    tcp: &TcpRepr<'_>,
) -> usize {
    let mut eth = EthernetFrame::new_unchecked(buf);
    eth.set_src_addr(src_mac);
    eth.set_dst_addr(dst_mac);
    eth.set_ethertype(EthernetProtocol::Ipv4);

    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.header_len() + tcp.payload.len(),
        hop_limit: 64,
    };
    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ipv4, &ChecksumCapabilities::default());

    let mut tcp_pkt = TcpPacket::new_unchecked(ipv4.payload_mut());
    tcp.emit(
        &mut tcp_pkt,
        &src_ip.into(),
        &dst_ip.into(),
        &ChecksumCapabilities::default(),
    );
    tcp_pkt.fill_checksum(&src_ip.into(), &dst_ip.into());

    ETHERNET_HEADER_LEN + ipv4.total_len() as usize
}

/// Parse a received Ethernet frame and extract the TCP repr and IPv4 addresses.
fn parse_tcp_packet(data: &[u8]) -> (Ipv4Address, Ipv4Address, TcpRepr<'_>) {
    let eth = EthernetFrame::new_unchecked(data);
    let ipv4 = Ipv4Packet::new_unchecked(eth.payload());
    let src_ip = ipv4.src_addr();
    let dst_ip = ipv4.dst_addr();
    let tcp_pkt = TcpPacket::new_unchecked(ipv4.payload());
    let tcp = TcpRepr::parse(
        &tcp_pkt,
        &src_ip.into(),
        &dst_ip.into(),
        &ChecksumCapabilities::default(),
    )
    .unwrap();
    (src_ip, dst_ip, tcp)
}

// ── Test harness ───────────────────────────────────────────────────

/// A test harness for exercising consomme's TCP stack end-to-end.
///
/// Encapsulates the consomme instance, a mock guest client, network
/// parameters, and a connected host socket. Provides helpers for
/// sending guest→host TCP segments and polling the stack.
struct TcpTestHarness {
    consomme: Consomme,
    client: TestClient,
    /// The accepted host-side TCP connection.
    host_stream: PolledSocket<std::net::TcpStream>,
    guest_mac: EthernetAddress,
    gateway_mac: EthernetAddress,
    guest_ip: Ipv4Address,
    dst_ip: Ipv4Address,
    guest_port: u16,
    dst_port: u16,
    /// Current guest send sequence number.
    guest_seq: TcpSeqNumber,
    /// ACK number for the server (learned from SYN-ACK).
    server_ack: TcpSeqNumber,
    buf: Vec<u8>,
}

impl TcpTestHarness {
    /// Create a harness and complete the TCP 3-way handshake.
    ///
    /// Starts a TCP listener on `127.0.0.1:0`, sends a SYN from the
    /// guest through consomme, waits for the host connect + SYN-ACK,
    /// and completes with an ACK. Returns the harness with an
    /// established connection ready for data transfer.
    async fn connect(driver: DefaultDriver) -> Self {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dst_port = std_listener.local_addr().unwrap().port();
        let mut listener = PolledSocket::new(&driver, std_listener).unwrap();

        let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
        let mut client = TestClient::new(driver);

        let guest_mac = consomme.params_mut().client_mac;
        let gateway_mac = consomme.params_mut().gateway_mac;
        let guest_ip = consomme.params_mut().client_ip;
        let dst_ip: Ipv4Address = Ipv4Addr::LOCALHOST;
        let guest_port = 44444u16;
        let guest_isn = TcpSeqNumber(1000);
        let mut buf = vec![0u8; 1514];

        // Guest sends SYN.
        let syn = TcpRepr {
            src_port: guest_port,
            dst_port,
            control: TcpControl::Syn,
            seq_number: guest_isn,
            ack_number: None,
            window_len: 64240,
            window_scale: Some(7),
            max_seg_size: Some(1460),
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };
        let len = build_tcp_packet(&mut buf, guest_mac, gateway_mac, guest_ip, dst_ip, &syn);
        consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE)
            .unwrap();

        // Poll until the host listener accepts the connection.
        let host_stream = std::future::poll_fn(|cx| {
            consomme.access(&mut client).poll(cx);
            let (stream, _) = std::task::ready!(listener.poll_accept(cx)).unwrap();
            Poll::Ready(PolledSocket::new(client.driver(), stream).unwrap())
        })
        .await;

        // Poll until consomme sends SYN-ACK to the guest.
        let received = client.received_packets.clone();
        std::future::poll_fn(|cx| {
            consomme.access(&mut client).poll(cx);
            let has_syn_ack = received.lock().iter().any(|p| {
                Self::is_tcp_packet(p)
                    .is_some_and(|t| t.control == TcpControl::Syn && t.ack_number.is_some())
            });
            if has_syn_ack {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        // Extract the server ISN from the SYN-ACK.
        let syn_ack_packet = client
            .received_packets
            .lock()
            .iter()
            .find(|p| {
                Self::is_tcp_packet(p)
                    .is_some_and(|t| t.control == TcpControl::Syn && t.ack_number.is_some())
            })
            .cloned()
            .expect("should have received SYN-ACK");

        let (_, _, syn_ack) = parse_tcp_packet(&syn_ack_packet);
        let server_ack = syn_ack.seq_number + 1;
        let guest_seq = guest_isn + 1; // SYN consumed 1 seq byte

        // Guest sends ACK to complete handshake.
        let mut harness = Self {
            consomme,
            client,
            host_stream,
            guest_mac,
            gateway_mac,
            guest_ip,
            dst_ip,
            guest_port,
            dst_port,
            guest_seq,
            server_ack,
            buf,
        };
        harness.send_segment(TcpControl::None, guest_seq, &[]);
        harness
    }

    /// Check if a raw Ethernet frame contains a TCP packet; return the
    /// parsed TcpRepr if so.
    fn is_tcp_packet(data: &[u8]) -> Option<TcpRepr<'_>> {
        if data.len() < ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + 20 {
            return None;
        }
        let eth = EthernetFrame::new_unchecked(data);
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        let ipv4 = Ipv4Packet::new_unchecked(eth.payload());
        if ipv4.next_header() != IpProtocol::Tcp {
            return None;
        }
        let tcp_pkt = TcpPacket::new_unchecked(ipv4.payload());
        TcpRepr::parse(
            &tcp_pkt,
            &ipv4.src_addr().into(),
            &ipv4.dst_addr().into(),
            &ChecksumCapabilities::default(),
        )
        .ok()
    }

    /// Send a TCP segment from the guest with the given control, sequence
    /// number, and payload. Uses the connection's ACK and window values.
    fn send_segment(&mut self, control: TcpControl, seq: TcpSeqNumber, payload: &[u8]) {
        let tcp = TcpRepr {
            src_port: self.guest_port,
            dst_port: self.dst_port,
            control,
            seq_number: seq,
            ack_number: Some(self.server_ack),
            window_len: 64240,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload,
        };
        let len = build_tcp_packet(
            &mut self.buf,
            self.guest_mac,
            self.gateway_mac,
            self.guest_ip,
            self.dst_ip,
            &tcp,
        );
        self.consomme
            .access(&mut self.client)
            .send(&self.buf[..len], &ChecksumState::NONE)
            .unwrap();
    }

    /// Send a data segment at the given sequence number. Shorthand for
    /// `send_segment(TcpControl::None, seq, payload)`.
    fn send_data(&mut self, seq: i32, payload: &[u8]) {
        self.send_segment(TcpControl::None, TcpSeqNumber(seq), payload);
    }

    /// Send a data segment at the current guest sequence number and
    /// advance it. For sending in-order data without tracking seq manually.
    fn send_data_next(&mut self, payload: &[u8]) {
        let seq = self.guest_seq;
        self.send_segment(TcpControl::None, seq, payload);
        self.guest_seq += payload.len();
    }

    /// Send a FIN at the current guest sequence number and advance it.
    fn send_fin(&mut self) {
        let seq = self.guest_seq;
        self.send_segment(TcpControl::Fin, seq, &[]);
        self.guest_seq += 1; // FIN consumes 1 seq byte
    }

    /// Send a FIN with data payload at the given sequence number.
    fn send_fin_with_data(&mut self, seq: i32, payload: &[u8]) {
        self.send_segment(TcpControl::Fin, TcpSeqNumber(seq), payload);
    }

    /// Poll consomme with the real async driver, reading from the host
    /// socket into `out` until at least `target_len` bytes are received.
    async fn poll_until_host_read(&mut self, out: &mut Vec<u8>, target_len: usize) {
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let host_stream = &mut self.host_stream;
        std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            let mut read_buf = [0u8; 4096];
            loop {
                match Pin::new(&mut *host_stream).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(0)) => break,
                    Poll::Ready(Ok(n)) => out.extend_from_slice(&read_buf[..n]),
                    Poll::Ready(Err(e)) => panic!("read error: {e}"),
                    Poll::Pending => break,
                }
            }
            if out.len() >= target_len {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Poll consomme with the real async driver until the host socket
    /// returns EOF (read returns 0). Collects all data into `out`.
    async fn poll_until_host_eof(&mut self, out: &mut Vec<u8>) {
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let host_stream = &mut self.host_stream;
        std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            let mut read_buf = [0u8; 4096];
            loop {
                match Pin::new(&mut *host_stream).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(0)) => return Poll::Ready(()),
                    Poll::Ready(Ok(n)) => out.extend_from_slice(&read_buf[..n]),
                    Poll::Ready(Err(e)) => panic!("read error: {e}"),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
    }

    /// Poll consomme with the real async driver and wait for a TCP packet
    /// sent to the guest that matches `filter`. Returns the raw packet.
    async fn poll_until_guest_packet(&mut self, filter: impl Fn(&TcpRepr<'_>) -> bool) -> Vec<u8> {
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let received = client.received_packets.clone();
        std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            let packets = received.lock();
            if let Some(p) = packets
                .iter()
                .rev()
                .find(|p| Self::is_tcp_packet(p).is_some_and(|t| filter(&t)))
            {
                Poll::Ready(p.clone())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    /// Write data from the host side into the connection.
    async fn host_write(&mut self, data: &[u8]) {
        use futures::AsyncWriteExt;
        self.host_stream.write_all(data).await.unwrap();
    }

    /// Shut down the host side write half (sends EOF to consomme).
    fn host_shutdown_write(&self) {
        self.host_stream.get().shutdown(Shutdown::Write).unwrap();
    }

    /// Clear captured guest packets so subsequent searches don't match old ones.
    fn clear_guest_packets(&mut self) {
        self.client.received_packets.lock().clear();
    }
}

// ── Tests ──────────────────────────────────────────────────────────

/// Test that in-order data sent from the guest arrives at the host socket.
#[pal_async::async_test]
async fn test_tcp_in_order_data(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.send_data_next(b"hello ");
    h.send_data_next(b"world");

    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, 11).await;

    assert_eq!(received, b"hello world");
}

/// Test that out-of-order segments are reassembled correctly.
///
/// Sends three data segments out of order (seg2, seg3, seg1) and
/// verifies the host socket receives them reassembled in order.
#[pal_async::async_test]
async fn test_tcp_ooo_reassembly(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    // guest_seq starts at 1001 (ISN 1000 + 1 for SYN).
    h.send_data(1006, b"BBBBB");
    h.send_data(1011, b"CCCCC");
    h.send_data(1001, b"AAAAA"); // fills gap, triggers reassembly

    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, 15).await;

    assert_eq!(
        received, b"AAAAABBBBBCCCCC",
        "host socket should receive reassembled data in order"
    );
}

/// Test that a FIN arriving after all data causes EOF on the host socket.
#[pal_async::async_test]
async fn test_tcp_fin_in_order(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.send_data_next(b"goodbye");
    h.send_fin();

    let mut received = Vec::new();
    h.poll_until_host_eof(&mut received).await;

    assert_eq!(received, b"goodbye");
}

/// Test that a FIN arriving before its preceding data is held until
/// the data gap is filled, then both data and EOF are delivered.
#[pal_async::async_test]
async fn test_tcp_fin_out_of_order(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    // Send FIN+data for the second segment (seq 1006..1011) before
    // the first segment (seq 1001..1006).
    h.send_fin_with_data(1006, b"WORLD");

    // The FIN should not be delivered yet. Send the missing data.
    h.send_data(1001, b"HELLO");

    let mut received = Vec::new();
    h.poll_until_host_eof(&mut received).await;

    assert_eq!(
        received, b"HELLOWORLD",
        "data should be reassembled and FIN delivered after gap is filled"
    );
}

/// Test that data sent from the host arrives at the guest as TCP segments.
#[pal_async::async_test]
async fn test_tcp_host_to_guest_data(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.clear_guest_packets();
    h.host_write(b"response data").await;

    // Wait for consomme to send a TCP data packet to the guest.
    let pkt = h.poll_until_guest_packet(|t| !t.payload.is_empty()).await;
    let (_, _, tcp) = parse_tcp_packet(&pkt);
    assert_eq!(tcp.payload, b"response data");
}

/// Test that a host-side EOF (shutdown write) causes consomme to send
/// a FIN to the guest.
#[pal_async::async_test]
async fn test_tcp_host_fin(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.clear_guest_packets();
    h.host_write(b"final").await;
    h.host_shutdown_write();

    // Wait for a FIN from consomme to the guest.
    let pkt = h
        .poll_until_guest_packet(|t| t.control == TcpControl::Fin)
        .await;
    let (_, _, tcp) = parse_tcp_packet(&pkt);
    // The FIN segment may carry the data payload or come after it.
    // Either way, verify we get a FIN.
    assert_eq!(tcp.control, TcpControl::Fin);
}

/// Test that a duplicate (retransmitted) segment is handled gracefully
/// and doesn't corrupt the data stream.
#[pal_async::async_test]
async fn test_tcp_duplicate_segment(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    // Send an OOO segment, then send it again (duplicate), then fill the gap.
    h.send_data(1006, b"BBBBB");
    // Retransmit the same OOO segment.
    h.send_data(1006, b"BBBBB");
    // Now fill the gap with the first segment.
    h.send_data(1001, b"AAAAA");

    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, 10).await;

    assert_eq!(
        received, b"AAAAABBBBB",
        "duplicate segment should not corrupt data"
    );
}

/// Test that a partially overlapping retransmission is handled correctly.
/// The overlapping region may contain data from either segment; the key
/// invariant is that the total length is correct and non-overlapping
/// regions are preserved.
#[pal_async::async_test]
async fn test_tcp_overlapping_retransmit(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    // Send an OOO segment: [1006..1011) = "BBBBB"
    h.send_data(1006, b"BBBBB");

    // Fill the gap with a segment that overlaps: [1001..1008) = "AAAAA##"
    // Bytes [1006..1008) are covered by both segments with different data.
    h.send_data(1001, b"AAAAA##");

    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, 10).await;

    assert_eq!(received.len(), 10);
    // Non-overlapping regions are deterministic.
    assert_eq!(&received[..5], b"AAAAA");
    assert_eq!(&received[7..10], b"BBB");
    // Bytes 5..7 are the overlap — could be "##" or "BB" depending on
    // write order. Either is acceptable; just verify no corruption.
    assert!(
        &received[5..7] == b"##" || &received[5..7] == b"BB",
        "overlap region should be from one segment or the other, got {:?}",
        &received[5..7]
    );
}

/// Test that `bind_tcp_port` registers a listener and that an external
/// TCP connection is forwarded to the guest as a SYN packet.
#[pal_async::async_test]
async fn test_tcp_bind_port_forward(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver.clone());

    let guest_port = 7777;
    let received = client.received_packets.clone();

    // Create and bind a TCP socket.
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let host_addr = socket.local_addr().unwrap().as_socket().unwrap();

    {
        let mut access = consomme.access(&mut client);
        access
            .bind_tcp_port(socket, guest_port)
            .expect("bind should succeed");

        assert!(
            access.inner.tcp.listeners.contains_key(&guest_port),
            "listener should be registered"
        );
    }

    // Connect from a host-side client to trigger the listener.
    let connector = std::net::TcpStream::connect(host_addr).unwrap();
    connector.set_nonblocking(true).unwrap();

    // Poll until consomme delivers a SYN to the guest.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        std::future::poll_fn(|cx| {
            consomme.access(&mut client).poll(cx);
            Poll::Ready(())
        })
        .await;

        let has_syn = received.lock().iter().any(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == guest_port)
        });
        if has_syn {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for forwarded TCP SYN"
        );
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(std::time::Duration::from_millis(10))
            .await;
    }

    // Verify the SYN targets the correct guest port.
    let packets = received.lock();
    let syn_pkt = packets
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == guest_port)
        })
        .expect("should have received a SYN");
    let (_, _, tcp) = parse_tcp_packet(syn_pkt);
    assert_eq!(tcp.dst_port, guest_port);
    assert_eq!(tcp.control, TcpControl::Syn);
}

/// Test that binding the same guest port twice returns `PortAlreadyBound`.
#[pal_async::async_test]
async fn test_tcp_bind_duplicate_port(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);

    let guest_port = 8888;

    let socket1 = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket1
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();

    let socket2_inst = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket2_inst
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();

    let mut access = consomme.access(&mut client);
    access
        .bind_tcp_port(socket1, guest_port)
        .expect("first bind should succeed");

    let err = access
        .bind_tcp_port(socket2_inst, guest_port)
        .expect_err("duplicate bind should fail");
    assert!(
        matches!(err, BindError::PortAlreadyBound(_)),
        "error should be PortAlreadyBound"
    );
}
