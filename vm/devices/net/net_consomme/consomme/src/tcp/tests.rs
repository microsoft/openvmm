// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::BindError;
use crate::ChecksumState;
use crate::Client;
use crate::Consomme;
use crate::ConsommeParams;
use crate::IpVersion;
use crate::PortForwardKey;
use futures::AsyncRead;
use futures::AsyncWrite;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use parking_lot::Mutex;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv4Repr;
use socket2::SockRef;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
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
        Self::connect_with_params(driver, ConsommeParams::new().unwrap()).await
    }

    /// Like [`connect`](Self::connect), but with caller-provided params, e.g.
    /// to set custom per-connection TCP buffer bounds.
    async fn connect_with_params(driver: DefaultDriver, params: ConsommeParams) -> Self {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dst_port = std_listener.local_addr().unwrap().port();
        let mut listener = PolledSocket::new(&driver, std_listener).unwrap();

        let mut consomme = Consomme::new({
            let mut params = params;
            params.allow_host_local_access = true;
            params
        });
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
    ///
    /// Polls consomme concurrently while writing so it can drain the host
    /// socket into `tx_buffer`. Without this, a write larger than the kernel
    /// socket buffer would block forever, since consomme is the only reader.
    /// Returns once all of `data` has been handed to the kernel socket.
    async fn host_write(&mut self, data: &[u8]) {
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let host_stream = &mut self.host_stream;
        let mut written = 0;
        std::future::poll_fn(move |cx| {
            // Drive consomme so it drains the host socket into the tx ring,
            // relieving backpressure on the write below.
            consomme.access(client).poll(cx);
            while written < data.len() {
                match Pin::new(&mut *host_stream).poll_write(cx, &data[written..]) {
                    Poll::Ready(Ok(0)) => panic!("host write returned 0"),
                    Poll::Ready(Ok(n)) => written += n,
                    Poll::Ready(Err(e)) => panic!("host write error: {e}"),
                    Poll::Pending => return Poll::Pending,
                }
            }
            Poll::Ready(())
        })
        .await;
    }

    /// Push `data` from the host side while polling consomme, returning as soon
    /// as `done` holds for the connection (even if not all of `data` has been
    /// written).
    ///
    /// This is needed when consomme intentionally stops reading the host socket
    /// (e.g. once the tx ring caps at `max`): the unread remainder stays in the
    /// kernel socket buffer, and a blocking `write_all` would deadlock. Polling
    /// consomme concurrently lets it drain the socket and reach the target
    /// state, which the caller observes via `done`.
    async fn host_write_until(
        &mut self,
        data: &[u8],
        mut done: impl FnMut(&TcpConnectionInner) -> bool,
    ) {
        let ft = self.four_tuple();
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let host_stream = &mut self.host_stream;
        let mut written = 0;
        std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            let inner = &consomme
                .tcp
                .connections
                .get(&ft)
                .expect("connection should exist")
                .inner;
            if done(inner) {
                return Poll::Ready(());
            }
            // Feed more data until the kernel socket buffer is full, then keep
            // polling consomme so it drains the socket and makes progress toward
            // `done`.
            while written < data.len() {
                match Pin::new(&mut *host_stream).poll_write(cx, &data[written..]) {
                    Poll::Ready(Ok(0)) => panic!("host write returned 0"),
                    Poll::Ready(Ok(n)) => written += n,
                    Poll::Ready(Err(e)) => panic!("host write error: {e}"),
                    Poll::Pending => break,
                }
            }
            Poll::Pending
        })
        .await;
    }

    /// Shut down the host side write half (sends EOF to consomme).
    fn host_shutdown_write(&self) {
        self.host_stream.get().shutdown(Shutdown::Write).unwrap();
    }

    /// Clear captured guest packets so subsequent searches don't match old ones.
    fn clear_guest_packets(&mut self) {
        self.client.received_packets.lock().clear();
    }

    /// The four-tuple identifying the established connection.
    fn four_tuple(&self) -> FourTuple {
        FourTuple {
            src: SocketAddr::V4(SocketAddrV4::new(self.guest_ip, self.guest_port)),
            dst: SocketAddr::V4(SocketAddrV4::new(self.dst_ip, self.dst_port)),
        }
    }

    /// Borrow the established connection's inner state for assertions.
    fn connection_inner(&self) -> &TcpConnectionInner {
        let ft = self.four_tuple();
        &self
            .consomme
            .tcp
            .connections
            .get(&ft)
            .expect("connection should exist")
            .inner
    }

    /// Poll consomme until `cond` holds for the established connection, leaving
    /// the future pending between polls so the async reactor can run and socket
    /// readiness can fire.
    async fn poll_until(&mut self, mut cond: impl FnMut(&TcpConnectionInner) -> bool) {
        let ft = self.four_tuple();
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        std::future::poll_fn(|cx| {
            consomme.access(client).poll(cx);
            let inner = &consomme
                .tcp
                .connections
                .get(&ft)
                .expect("connection should exist")
                .inner;
            if cond(inner) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Flood guest→host data (without reading the host socket) until the
    /// available receive buffer (`rx_window_avail`) reaches zero, which drives
    /// the advertised window to zero. Only sends segments that fit the current
    /// window, so nothing is dropped. Returns bytes accepted.
    async fn flood_guest_until_window_closed(&mut self) -> usize {
        let guest_mac = self.guest_mac;
        let gateway_mac = self.gateway_mac;
        let guest_ip = self.guest_ip;
        let dst_ip = self.dst_ip;
        let guest_port = self.guest_port;
        let dst_port = self.dst_port;
        let server_ack = self.server_ack;
        let ft = self.four_tuple();

        let consomme = &mut self.consomme;
        let client = &mut self.client;
        // Deliberately do NOT read `host_stream`: the path backs up so the
        // receive window fills.
        let guest_seq = &mut self.guest_seq;
        let mut buf = vec![0u8; 1514];
        let payload = [0x5Au8; 1400];
        let mut total = 0usize;
        // Safety cap so a regression can't loop forever.
        let mut budget = 64 << 20;
        std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            let avail = consomme
                .tcp
                .connections
                .get(&ft)
                .expect("connection should exist")
                .inner
                .rx_window_avail();
            if avail == 0 || budget == 0 {
                return Poll::Ready(total);
            }
            let n = avail.min(payload.len()).min(budget);
            let tcp = TcpRepr {
                src_port: guest_port,
                dst_port,
                control: TcpControl::None,
                seq_number: *guest_seq,
                ack_number: Some(server_ack),
                window_len: 64240,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &payload[..n],
            };
            let len = build_tcp_packet(&mut buf, guest_mac, gateway_mac, guest_ip, dst_ip, &tcp);
            consomme
                .access(client)
                .send(&buf[..len], &ChecksumState::NONE)
                .unwrap();
            *guest_seq += n;
            total += n;
            budget -= n;
            // Re-poll promptly to keep pushing; once the host socket buffers
            // fill, consomme stops draining and `avail` reaches zero.
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await
    }

    /// Drain the host socket while polling consomme, waiting for a guest-bound
    /// ACK that advertises a non-zero receive window. Returns the advertised
    /// window length, or `None` if no such packet appeared within a few
    /// seconds.
    async fn drain_host_until_window_update(&mut self) -> Option<u16> {
        let mut timer = pal_async::timer::PolledTimer::new(self.client.driver());
        let received = self.client.received_packets.clone();
        let consomme = &mut self.consomme;
        let client = &mut self.client;
        let host_stream = &mut self.host_stream;
        let poll = std::future::poll_fn(move |cx| {
            consomme.access(client).poll(cx);
            // Drain the host socket so the receive window reopens. Registers a
            // read-readiness waker on Pending so we are re-polled as more data
            // arrives.
            let mut rb = [0u8; 4096];
            loop {
                match Pin::new(&mut *host_stream).poll_read(cx, &mut rb) {
                    Poll::Ready(Ok(0)) => break,
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(e)) => panic!("host read error: {e}"),
                    Poll::Pending => break,
                }
            }
            let found = received.lock().iter().rev().find_map(|p| {
                let t = TcpTestHarness::is_tcp_packet(p)?;
                (t.ack_number.is_some() && t.window_len > 0).then_some(t.window_len)
            });
            match found {
                Some(w) => Poll::Ready(w),
                None => Poll::Pending,
            }
        });
        let timeout = timer.sleep(std::time::Duration::from_secs(5));
        let poll = std::pin::pin!(poll);
        let timeout = std::pin::pin!(timeout);
        match futures::future::select(poll, timeout).await {
            futures::future::Either::Left((w, _)) => Some(w),
            futures::future::Either::Right(_) => None,
        }
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

/// Guest-bound data segments must carry a correct TCP checksum computed over
/// the populated payload (not skipped, not stale over a zeroed payload).
#[pal_async::async_test]
async fn test_tcp_guest_packet_checksum_valid(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.clear_guest_packets();
    h.host_write(b"checksum me").await;

    let pkt = h.poll_until_guest_packet(|t| !t.payload.is_empty()).await;
    let eth = EthernetFrame::new_unchecked(&pkt[..]);
    let ipv4 = Ipv4Packet::new_unchecked(eth.payload());
    let src_ip = ipv4.src_addr();
    let dst_ip = ipv4.dst_addr();
    let tcp_pkt = TcpPacket::new_unchecked(ipv4.payload());

    assert!(
        tcp_pkt.verify_checksum(&src_ip.into(), &dst_ip.into()),
        "guest-bound TCP segment must have a valid checksum"
    );
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
            access
                .inner
                .tcp
                .listeners
                .contains_key(&PortForwardKey::new(IpVersion::Ipv4, guest_port)),
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

/// Test that when a loopback connection is forwarded to the guest, the source
/// IP is rewritten from loopback to a virtual address within the subnet (not
/// the raw 127.0.0.1), ensuring the guest routes its reply through the virtual
/// adapter.
#[pal_async::async_test]
async fn test_tcp_port_forward_loopback_src_rewritten(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver.clone());

    let guest_port = 9999;
    let received = client.received_packets.clone();
    let client_ip = consomme.params_mut().client_ip;

    // Create and bind a TCP socket on loopback.
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
    }

    // Connect from localhost to trigger the listener.
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

    // Verify the source IP of the forwarded SYN is NOT loopback and NOT the
    // guest's own IP (it should be a virtual address in the subnet).
    let packets = received.lock();
    let syn_pkt = packets
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == guest_port)
        })
        .expect("should have received a SYN");
    let (src_ip, dst_ip, _tcp) = parse_tcp_packet(syn_pkt);

    // The destination should be the guest.
    assert_eq!(dst_ip, client_ip);
    // The source must not be loopback (127.x.x.x) since that would cause the
    // guest to route the reply via its own loopback interface.
    assert!(
        !src_ip.is_loopback(),
        "forwarded SYN source IP should not be loopback, got {src_ip}"
    );
    // The source must not be the guest's own IP either.
    assert_ne!(
        src_ip, client_ip,
        "forwarded SYN source IP should not be the guest's own IP"
    );
}

/// Test that when the consomme endpoint is itself a loopback adapter (its own
/// `client_ip` is a loopback address, as used by WSL's VirtioProxy localhost
/// forwarding), the source IP of a forwarded loopback connection is left as
/// loopback and is **not** rewritten to a virtual subnet address. Such adapters
/// rely on the loopback source staying in range so the guest routes the reply
/// back through the adapter.
#[pal_async::async_test]
async fn test_tcp_port_forward_loopback_adapter_src_not_rewritten(driver: DefaultDriver) {
    let mut params = ConsommeParams::new().unwrap();
    // Configure this endpoint as a loopback adapter.
    params.client_ip = Ipv4Address::new(127, 0, 0, 1);
    let mut consomme = Consomme::new(params);
    let mut client = TestClient::new(driver.clone());

    let guest_port = 9999;
    let received = client.received_packets.clone();
    let client_ip = consomme.params_mut().client_ip;

    // Create and bind a TCP socket on loopback.
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
    }

    // Connect from localhost to trigger the listener.
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

    // Verify the source IP of the forwarded SYN is left as loopback (not
    // rewritten to a virtual subnet address).
    let packets = received.lock();
    let syn_pkt = packets
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == guest_port)
        })
        .expect("should have received a SYN");
    let (src_ip, dst_ip, _tcp) = parse_tcp_packet(syn_pkt);

    // The destination should be the (loopback) guest IP.
    assert_eq!(dst_ip, client_ip);
    // The source must remain loopback so the guest's reply is routed back
    // through the loopback adapter rather than out the default interface.
    assert!(
        src_ip.is_loopback(),
        "forwarded SYN source IP should remain loopback, got {src_ip}"
    );
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

/// Test that the same guest TCP port can be bound separately for IPv4 and IPv6.
#[pal_async::async_test]
async fn test_tcp_bind_same_port_different_families(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);

    let guest_port = 8889;

    let socket_v4 = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket_v4
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();

    let socket_v6 = Socket::new(Domain::IPV6, Type::STREAM, None).unwrap();
    socket_v6.set_only_v6(true).unwrap();
    match socket_v6.bind(&SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0).into()) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(err) => panic!("IPv6 bind failed: {err}"),
    }

    let mut access = consomme.access(&mut client);
    access
        .bind_tcp_port(socket_v4, guest_port)
        .expect("IPv4 bind should succeed");
    access
        .bind_tcp_port(socket_v6, guest_port)
        .expect("IPv6 bind should succeed");

    access
        .unbind_tcp_port(IpVersion::Ipv4, guest_port)
        .expect("IPv4 unbind should succeed");
    assert!(
        access
            .inner
            .tcp
            .listeners
            .contains_key(&PortForwardKey::new(IpVersion::Ipv6, guest_port)),
        "IPv6 listener should remain registered"
    );
}

/// Test that deferred ACKs are flushed during poll cycles.
///
/// When the guest sends data, the ACK is deferred (not emitted
/// immediately in the packet-processing path). On the next poll cycle,
/// the ACK must be flushed so the peer doesn't retransmit.
#[pal_async::async_test]
async fn test_tcp_deferred_ack_flush(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    h.clear_guest_packets();

    // Send data from the guest to the host. This triggers an ACK
    // from consomme back to the guest, but via the deferred ACK
    // mechanism it should be flushed during the poll cycle.
    h.send_data_next(b"ping");

    // Poll until the host receives the data (which exercises the
    // poll cycle that should flush the deferred ACK).
    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, 4).await;
    assert_eq!(received, b"ping");

    // Verify that an ACK was sent back to the guest acknowledging
    // the data (seq advanced past the SYN-ACK's ack).
    let guest_seq_after = h.guest_seq;
    let pkt = h
        .poll_until_guest_packet(|t| t.ack_number.is_some_and(|ack| ack >= guest_seq_after))
        .await;
    let (_, _, tcp) = parse_tcp_packet(&pkt);
    assert!(
        tcp.ack_number.unwrap() >= guest_seq_after,
        "deferred ACK should acknowledge the guest data"
    );
}

/// Test that a burst of guest packets produces a single consolidated ACK.
///
/// The ACK deferral mechanism (AckPolicy::Defer in handle_tcp) prevents
/// emitting a pure ACK for every individual guest packet. Instead, a
/// single consolidated ACK covering the entire burst is sent during the
/// poll cycle (AckPolicy::Flush in poll_socket_backend). This test
/// verifies that sending N data segments back-to-back results in at most
/// one pure ACK rather than N pure ACKs.
#[pal_async::async_test]
async fn test_tcp_deferred_ack_batching(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;

    // Complete the handshake poll cycle so any pending handshake ACK is
    // flushed before we start counting.
    std::future::poll_fn(|cx| {
        h.consomme.access(&mut h.client).poll(cx);
        Poll::Ready(())
    })
    .await;
    h.clear_guest_packets();

    // Send a burst of 5 data segments back-to-back. Each call to
    // `send` invokes `handle_tcp` → `send_next(Defer)`, which should
    // NOT emit a pure ACK.
    for i in 0..5 {
        let payload = format!("seg{i}");
        h.send_data_next(payload.as_bytes());
    }

    // At this point, no poll cycle has run, so no ACK should have been
    // emitted yet — only the Defer path in handle_tcp was exercised.
    let pure_acks_before_poll: usize = h
        .client
        .received_packets
        .lock()
        .iter()
        .filter(|p| {
            TcpTestHarness::is_tcp_packet(p).is_some_and(|t| {
                t.payload.is_empty() && t.control == TcpControl::None && t.ack_number.is_some()
            })
        })
        .count();
    assert_eq!(
        pure_acks_before_poll, 0,
        "no pure ACKs should be emitted during handle_tcp (Defer policy)"
    );

    // Now poll — this runs poll_socket_backend which flushes with
    // AckPolicy::Flush, emitting at most one consolidated ACK.
    let total_payload_len = "seg0seg1seg2seg3seg4".len();
    let mut received = Vec::new();
    h.poll_until_host_read(&mut received, total_payload_len)
        .await;
    assert_eq!(received, b"seg0seg1seg2seg3seg4");

    // Count pure ACKs (no payload, no SYN/FIN) sent to the guest.
    let pure_acks: Vec<_> = h
        .client
        .received_packets
        .lock()
        .iter()
        .filter(|p| {
            TcpTestHarness::is_tcp_packet(p).is_some_and(|t| {
                t.payload.is_empty() && t.control == TcpControl::None && t.ack_number.is_some()
            })
        })
        .cloned()
        .collect();

    // We expect exactly 1 consolidated ACK, not 5.
    assert!(
        pure_acks.len() <= 2,
        "expected at most 2 pure ACKs for a 5-segment burst (got {}); \
         the deferred ACK mechanism should consolidate per-packet ACKs",
        pure_acks.len()
    );

    // The consolidated ACK should acknowledge ALL 5 segments.
    let final_guest_seq = h.guest_seq;
    let last_ack = pure_acks.last().expect("should have at least one ACK");
    let (_, _, tcp) = parse_tcp_packet(last_ack);
    assert!(
        tcp.ack_number.unwrap() >= final_guest_seq,
        "consolidated ACK should cover the entire burst: expected ack >= {}, got {}",
        final_guest_seq.0,
        tcp.ack_number.unwrap().0,
    );
}

/// Test that window scaling is not applied to SYN-ACK window fields
/// but is applied after the handshake completes.
///
/// RFC 1323 §2.2: the window scale option takes effect only after
/// the three-way handshake is complete. The SYN and SYN-ACK window
/// fields represent unscaled values.
#[pal_async::async_test]
async fn test_tcp_window_scale_activation(driver: DefaultDriver) {
    let mut consomme = Consomme::new({
        let mut params = ConsommeParams::new().unwrap();
        params.allow_host_local_access = true;
        params
    });
    let mut client = TestClient::new(driver.clone());

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;
    let dst_ip: Ipv4Address = Ipv4Addr::LOCALHOST;
    let guest_port = 55555u16;
    let guest_isn = TcpSeqNumber(2000);
    let mut buf = vec![0u8; 1514];

    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dst_port = std_listener.local_addr().unwrap().port();
    let mut listener = PolledSocket::new(&driver, std_listener).unwrap();

    // Guest sends SYN with window_scale=7.
    let syn = TcpRepr {
        src_port: guest_port,
        dst_port,
        control: TcpControl::Syn,
        seq_number: guest_isn,
        ack_number: None,
        window_len: 512, // Small unscaled window in the SYN
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

    // Poll until the host listener accepts and consomme sends SYN-ACK.
    let received = client.received_packets.clone();
    let _host_stream = std::future::poll_fn(|cx| {
        consomme.access(&mut client).poll(cx);
        let (stream, _) = std::task::ready!(listener.poll_accept(cx)).unwrap();
        Poll::Ready(PolledSocket::new(client.driver(), stream).unwrap())
    })
    .await;

    std::future::poll_fn(|cx| {
        consomme.access(&mut client).poll(cx);
        let has_syn_ack = received.lock().iter().any(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.ack_number.is_some())
        });
        if has_syn_ack {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    // Extract the SYN-ACK and verify:
    // 1. window_scale option is present (window scaling was negotiated)
    // 2. window_len is the unscaled value (fits in u16 without shift)
    let syn_ack_pkt = received
        .lock()
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.ack_number.is_some())
        })
        .cloned()
        .expect("should have received SYN-ACK");

    let (_, _, syn_ack) = parse_tcp_packet(&syn_ack_pkt);
    // The SYN-ACK must include window_scale option since the SYN had one.
    assert!(
        syn_ack.window_scale.is_some(),
        "SYN-ACK should include window_scale option when SYN had one"
    );
    let syn_ack_window_scale = syn_ack.window_scale.unwrap();
    // RFC 7323 §2.2: the window field in a SYN/SYN-ACK is NOT scaled, even
    // though the window_scale option is present. The guest reads window_len
    // verbatim, so it must carry the real receive window (the 16 KiB initial
    // cap), not a pre-shifted value. If it were pre-scaled (>> 7), the guest
    // would see only 128 bytes and stall at connection start.
    assert_eq!(
        syn_ack.window_len as usize,
        16 * 1024,
        "SYN-ACK must advertise the unscaled receive window (scale={syn_ack_window_scale})",
    );

    // Now complete the handshake with an ACK that has a small window.
    // This exercises the post-handshake path where window scaling IS applied.
    let server_ack = syn_ack.seq_number + 1;
    let guest_seq = guest_isn + 1;
    let ack = TcpRepr {
        src_port: guest_port,
        dst_port,
        control: TcpControl::None,
        seq_number: guest_seq,
        ack_number: Some(server_ack),
        // Advertise a small unscaled window value. With scale=7, the
        // effective window should be 100 << 7 = 12800 bytes.
        window_len: 100,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let len = build_tcp_packet(&mut buf, guest_mac, gateway_mac, guest_ip, dst_ip, &ack);
    consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE)
        .unwrap();

    // Poll to process the ACK (completing the handshake).
    std::future::poll_fn(|cx| {
        consomme.access(&mut client).poll(cx);
        Poll::Ready(())
    })
    .await;

    // Verify window scaling is applied after the handshake by checking
    // internal connection state. The guest advertised window_len=100 with
    // scale=7, so the effective tx window should be 100 << 7 = 12800.
    let ft = FourTuple {
        src: SocketAddr::V4(SocketAddrV4::new(guest_ip, guest_port)),
        dst: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, dst_port)),
    };
    let conn = consomme
        .tcp
        .connections
        .get(&ft)
        .expect("connection should exist");
    assert!(
        conn.inner.tx_window_scale_active,
        "window scaling should be active after handshake completes"
    );
    assert_eq!(conn.inner.tx_window_scale, 7);
    assert_eq!(conn.inner.tx_window_len, 100);
    // The effective window used for send decisions:
    let effective_window = (conn.inner.tx_window_len as usize) << conn.inner.tx_window_scale;
    assert_eq!(
        effective_window, 12800,
        "effective window should be window_len << scale after handshake"
    );
}

/// Test that the host-initiated (port-forward) path does NOT apply window
/// scaling to the SYN-ACK window field.
///
/// In the port-forward path, consomme sends a SYN to the guest, and the
/// guest replies with a SYN-ACK whose window field is unscaled per
/// RFC 1323 §2.2. After `handle_listen_syn` stores this unscaled window
/// and transitions to Established, `tx_window_scale_active` must remain
/// false until the guest sends a non-SYN segment that triggers the
/// "Update send window" block in `handle_packet`. This prevents
/// consomme from sending beyond the guest's actual receive window.
#[pal_async::async_test]
async fn test_tcp_port_forward_window_scale_guard(driver: DefaultDriver) {
    use std::io::Write;

    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver.clone());

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;
    let guest_port = 7777u16;
    let received = client.received_packets.clone();
    let mut buf = vec![0u8; 1514];

    // Set up a port-forward listener.
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let host_addr = socket.local_addr().unwrap().as_socket().unwrap();

    consomme
        .access(&mut client)
        .bind_tcp_port(socket, guest_port)
        .expect("bind should succeed");

    // Connect from the host side to trigger the port-forward SYN.
    let mut connector = std::net::TcpStream::connect(host_addr).unwrap();
    connector.set_nonblocking(true).unwrap();

    // Poll until consomme sends a SYN to the guest.
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
            "timed out waiting for SYN"
        );
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(std::time::Duration::from_millis(10))
            .await;
    }

    // Extract the SYN from consomme (which includes window_scale option).
    let syn_pkt = received
        .lock()
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == guest_port)
        })
        .cloned()
        .expect("should have SYN");
    let (syn_src_ip, _, syn_tcp) = parse_tcp_packet(&syn_pkt);
    let server_isn = syn_tcp.seq_number;
    let server_window_scale = syn_tcp.window_scale.unwrap_or(0);
    assert!(
        server_window_scale > 0,
        "server should offer window scaling"
    );

    // Guest replies with SYN-ACK. Advertise a small unscaled window (200
    // bytes) with window_scale=7 offered. The SYN-ACK window is unscaled
    // per RFC 1323, so consomme must treat 200 as the actual byte limit
    // until the first post-handshake window update.
    let guest_isn = TcpSeqNumber(5000);
    let syn_ack = TcpRepr {
        src_port: guest_port,
        dst_port: syn_tcp.src_port,
        control: TcpControl::Syn,
        seq_number: guest_isn,
        ack_number: Some(server_isn + 1),
        window_len: 200, // Unscaled: actual receive window is 200 bytes
        window_scale: Some(7),
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let len = build_tcp_packet(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        syn_src_ip,
        &syn_ack,
    );
    consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE)
        .unwrap();

    // Poll to let consomme process the SYN-ACK (handle_listen_syn).
    std::future::poll_fn(|cx| {
        consomme.access(&mut client).poll(cx);
        Poll::Ready(())
    })
    .await;

    // Verify internal state: tx_window_scale_active should be FALSE
    // because handle_listen_syn doesn't activate it.
    let ft = FourTuple {
        src: SocketAddr::V4(SocketAddrV4::new(guest_ip, guest_port)),
        dst: SocketAddr::V4(SocketAddrV4::new(syn_src_ip, syn_tcp.src_port)),
    };
    let conn = consomme
        .tcp
        .connections
        .get(&ft)
        .expect("connection should exist after SYN-ACK");
    assert!(
        !conn.inner.tx_window_scale_active,
        "tx_window_scale_active must be false after handle_listen_syn; \
         the SYN-ACK window is unscaled"
    );
    assert_eq!(conn.inner.tx_window_len, 200);
    assert_eq!(conn.inner.tx_window_scale, 7);

    // Write more data than 200 bytes from the host side. If window
    // scaling were incorrectly applied, consomme would treat the window
    // as 200 << 7 = 25600 bytes and send all of it. With the guard,
    // it should only send up to 200 bytes.
    let host_data = vec![0xABu8; 1000];
    connector.write_all(&host_data).unwrap();

    // Clear received packets so we only see new data segments.
    received.lock().clear();

    // Poll multiple cycles to let consomme read from the host socket
    // and send data to the guest. The host socket needs to become
    // readable first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        std::future::poll_fn(|cx| {
            consomme.access(&mut client).poll(cx);
            Poll::Ready(())
        })
        .await;

        let has_data = received.lock().iter().any(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.dst_port == guest_port && !t.payload.is_empty())
        });
        if has_data {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for host→guest data"
        );
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(std::time::Duration::from_millis(10))
            .await;
    }

    // Count total payload bytes sent to the guest. With the unscaled
    // window of 200, consomme should send at most 200 bytes.
    let total_payload_sent: usize = received
        .lock()
        .iter()
        .filter_map(|p| TcpTestHarness::is_tcp_packet(p))
        .filter(|t| t.dst_port == guest_port && !t.payload.is_empty())
        .map(|t| t.payload.len())
        .sum();

    assert!(
        total_payload_sent <= 200,
        "with unscaled SYN-ACK window of 200, consomme should send at most \
         200 bytes before window scaling is activated, but sent {total_payload_sent}"
    );
    assert!(
        total_payload_sent > 0,
        "consomme should send at least some data"
    );
}

/// Test that the TCP loopback port remapping works end-to-end:
/// when the guest sends a SYN to localhost on a listener port, consomme
/// proxies the connection through the host listener, and the returned SYN
/// back to the guest has the correct source port (the guest's original
/// source port, not the proxy ephemeral port).
#[pal_async::async_test]
async fn test_tcp_loopback_port_remap(driver: DefaultDriver) {
    let mut consomme = Consomme::new({
        let mut params = ConsommeParams::new().unwrap();
        params.allow_host_local_access = true;
        params
    });
    let mut client = TestClient::new(driver.clone());

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;
    let listener_guest_port = 8080u16;
    let guest_src_port = 55555u16;
    let dst_ip: Ipv4Address = Ipv4Addr::LOCALHOST;

    let received = client.received_packets.clone();

    // Bind a TCP listener on an ephemeral host port, mapped to guest port 8080.
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();

    {
        let mut access = consomme.access(&mut client);
        access
            .bind_tcp_port(socket, listener_guest_port)
            .expect("bind should succeed");
    }

    // Guest sends a SYN to 127.0.0.1 on the listener port. This simulates
    // the guest trying to connect to a host service that is also forwarded
    // back to the guest (loopback through consomme).
    let syn = TcpRepr {
        src_port: guest_src_port,
        dst_port: listener_guest_port,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(2000),
        ack_number: None,
        window_len: 64240,
        window_scale: Some(7),
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let mut buf = vec![0u8; 1514];
    let len = build_tcp_packet(&mut buf, guest_mac, gateway_mac, guest_ip, dst_ip, &syn);
    consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE)
        .unwrap();

    // Poll until consomme delivers the loopback SYN back to the guest on the
    // listener port. The source port in that SYN should be the guest's
    // original source port (55555), not the proxy's ephemeral port.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        std::future::poll_fn(|cx| {
            consomme.access(&mut client).poll(cx);
            Poll::Ready(())
        })
        .await;

        // Look for a SYN targeting the listener_guest_port as destination.
        let has_loopback_syn = received.lock().iter().any(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == listener_guest_port)
        });
        if has_loopback_syn {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for loopback TCP SYN to be forwarded back to guest"
        );
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(std::time::Duration::from_millis(10))
            .await;
    }

    // Find the SYN that was forwarded to the guest on the listener port.
    let packets = received.lock();
    let loopback_syn = packets
        .iter()
        .find(|p| {
            TcpTestHarness::is_tcp_packet(p)
                .is_some_and(|t| t.control == TcpControl::Syn && t.dst_port == listener_guest_port)
        })
        .expect("should have received a loopback SYN");
    let (src_ip, recv_dst_ip, tcp) = parse_tcp_packet(loopback_syn);

    // The destination should be the guest's IP.
    assert_eq!(recv_dst_ip, guest_ip);
    // The source port should be the guest's original source port (remapped
    // from the proxy's ephemeral port back to the guest port).
    assert_eq!(
        tcp.src_port, guest_src_port,
        "loopback SYN source port should be the guest's original source port \
         ({guest_src_port}), not a proxy ephemeral port; got {}",
        tcp.src_port
    );
    // Source IP should not be loopback.
    assert!(
        !src_ip.is_loopback(),
        "loopback SYN source IP should not be 127.x.x.x, got {src_ip}"
    );
}

/// `NormalizedBufferBounds::from_bounds` clamps to `[16 KiB, 4 MiB]`, rounds up
/// to a power of two, and keeps `initial <= max`.
#[test]
fn test_normalized_buffer_bounds() {
    use crate::TcpBufferBounds;
    let n = |initial, max| NormalizedBufferBounds::from_bounds(TcpBufferBounds { initial, max });
    // Clamp up to the 16 KiB floor.
    let b = n(1, 1);
    assert_eq!((b.initial, b.max), (16 << 10, 16 << 10));
    // Clamp down to the 4 MiB ceiling.
    let b = n(64 << 20, 64 << 20);
    assert_eq!((b.initial, b.max), (4 << 20, 4 << 20));
    // Round non-powers-of-two up.
    let b = n(100 << 10, 100 << 10);
    assert_eq!((b.initial, b.max), (128 << 10, 128 << 10));
    // initial is clamped to be no greater than max.
    let b = n(4 << 20, 64 << 10);
    assert_eq!((b.initial, b.max), (64 << 10, 64 << 10));
}

/// The rx window scale derived from `max` must let the advertised receive
/// window reach `max` without renegotiating window scaling mid-connection.
#[pal_async::async_test]
async fn test_tcp_rx_window_scale_reaches_max(driver: DefaultDriver) {
    let h = TcpTestHarness::connect(driver).await;
    let c = h.connection_inner();
    assert_eq!(c.rx_buffer_max, 4 << 20, "default rx max should be 4 MiB");
    assert!(
        c.rx_window_scale > 0,
        "window scaling must be enabled to grow past 64 KiB"
    );
    let max_advertisable = (u16::MAX as usize) << c.rx_window_scale;
    assert!(
        max_advertisable >= c.rx_buffer_max,
        "advertised window ceiling {max_advertisable} must reach rx max {}",
        c.rx_buffer_max,
    );
}

/// Autotune: the tx ring grows past its initial size when the host floods data
/// faster than the guest ACKs, and stays a power of two within `max`.
#[pal_async::async_test]
async fn test_tcp_tx_buffer_autotune_grows(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect(driver).await;
    let initial = h.connection_inner().tx_buffer.capacity();

    // Flood the host->guest direction without ever ACKing from the guest, so
    // the unacked data piles up in the tx ring and forces it to grow.
    let payload = vec![0xABu8; 64 << 10];
    h.host_write(&payload).await;
    h.poll_until(|c| c.tx_buffer.capacity() > initial).await;

    let cap = h.connection_inner().tx_buffer.capacity();
    assert!(
        cap > initial,
        "tx ring should have grown past {initial}, got {cap}"
    );
    assert!(
        cap.is_power_of_two(),
        "tx ring capacity must stay a power of two: {cap}"
    );
    assert!(
        cap <= 4 << 20,
        "tx ring must not exceed the 4 MiB ceiling: {cap}"
    );
}

/// Autotune: tx ring growth stops at the configured `max` and never exceeds it.
#[pal_async::async_test]
async fn test_tcp_tx_buffer_autotune_caps_at_max(driver: DefaultDriver) {
    let mut params = ConsommeParams::new().unwrap();
    // Small ceiling so a modest flood saturates it.
    params.tcp_tx_buffer = crate::TcpBufferBounds {
        initial: 16 << 10,
        max: 32 << 10,
    };
    let mut h = TcpTestHarness::connect_with_params(driver, params).await;

    // 64 KiB exceeds the 32 KiB ceiling; consomme only ingests up to the cap
    // (the rest stays in the host socket buffer). Stop writing as soon as the
    // ring caps so the unread remainder can't block the write.
    let payload = vec![0xABu8; 64 << 10];
    h.host_write_until(&payload, |c| c.tx_buffer.capacity() >= 32 << 10)
        .await;

    let cap = h.connection_inner().tx_buffer.capacity();
    assert_eq!(
        cap,
        32 << 10,
        "tx ring must cap at the configured 32 KiB max"
    );
}

/// Regression test: after the guest egress fills the receive window and it
/// later reopens (host drains), consomme must send an unsolicited window-update
/// ACK rather than waiting for the guest's zero-window probe.
#[pal_async::async_test]
async fn test_tcp_zero_window_reopen_sends_update(driver: DefaultDriver) {
    let mut params = ConsommeParams::new().unwrap();
    // Small, fixed receive window so it closes quickly and autotune can't grow
    // it (which would mask the reopen path via its own `needs_ack`).
    params.tcp_rx_buffer = crate::TcpBufferBounds {
        initial: 16 << 10,
        max: 16 << 10,
    };
    let mut h = TcpTestHarness::connect_with_params(driver, params).await;

    // Shrink the host receive buffer so the egress path backs up after only a
    // modest amount of data, keeping the flood bounded.
    SockRef::from(h.host_stream.get())
        .set_recv_buffer_size(8 << 10)
        .unwrap();

    // Flood until consomme's advertised receive window closes to zero.
    let sent = h.flood_guest_until_window_closed().await;
    assert_eq!(
        h.connection_inner().rx_window_avail(),
        0,
        "receive window should have closed after flooding {sent} bytes"
    );
    assert!(
        h.connection_inner().rx_window_last_adv < h.connection_inner().tx_mss,
        "consomme should have advertised a (near) zero window to the guest"
    );

    // Discard the zero-window ACKs, then drain the host socket. Consomme must
    // emit a window-update ACK on its own, without the guest probing.
    h.clear_guest_packets();
    let window_update = h.drain_host_until_window_update().await;
    assert!(
        window_update.is_some_and(|w| w > 0),
        "consomme must proactively re-advertise a non-zero window after the \
         host drains the backlog; got {window_update:?}"
    );
}

/// Verifies that the zero-copy TCP checksum computed over the header and the
/// two payload slices — combined with the pseudo-header — matches smoltcp's
/// contiguous checksum, for every payload split point (which exercises the
/// odd-length `a` boundary that byte-swaps `b`).
#[test]
fn tcp_checksum_matches_smoltcp() {
    use smoltcp::wire::IpAddress;
    use smoltcp::wire::IpProtocol;
    use smoltcp::wire::Ipv6Address;
    use smoltcp::wire::TcpControl;
    use smoltcp::wire::TcpPacket;
    use smoltcp::wire::TcpRepr;
    use smoltcp::wire::TcpSeqNumber;

    let v4 = (IpAddress::v4(93, 184, 216, 34), IpAddress::v4(10, 0, 0, 2));
    let v6 = (
        IpAddress::Ipv6(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x1)),
        IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x2)),
    );

    for (src, dst) in [v4, v6] {
        for payload_len in [0usize, 1, 2, 3, 4, 5, 15, 16, 17, 63, 1460] {
            let payload: Vec<u8> = (0..payload_len).map(|i| (i * 7 + 3) as u8).collect();
            let repr = TcpRepr {
                src_port: 443,
                dst_port: 51000,
                control: TcpControl::None,
                seq_number: TcpSeqNumber(0x1234_5678),
                ack_number: Some(TcpSeqNumber(0x7654_4321)),
                window_len: 65535,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &payload,
            };
            let mut buf = vec![0u8; repr.buffer_len()];
            let mut pkt = TcpPacket::new_unchecked(&mut buf);
            repr.emit(&mut pkt, &src, &dst, &Default::default());
            pkt.set_checksum(0);
            pkt.fill_checksum(&src, &dst);
            let reference = pkt.checksum();

            let header_len = pkt.header_len() as usize;
            pkt.set_checksum(0);
            for split in 0..=payload_len {
                let (a, b) = payload.split_at(split);
                let got = !checksum::combine(&[
                    checksum::pseudo_header(
                        &src,
                        &dst,
                        IpProtocol::Tcp,
                        (header_len + payload_len) as u32,
                    ),
                    checksum::data(&pkt.as_ref()[..header_len]),
                    checksum::data(a),
                    if a.len() % 2 == 0 {
                        checksum::data(b)
                    } else {
                        checksum::data(b).swap_bytes()
                    },
                ]);
                assert_eq!(
                    got, reference,
                    "checksum mismatch: payload_len={payload_len} split={split}"
                );
            }
        }
    }
}
