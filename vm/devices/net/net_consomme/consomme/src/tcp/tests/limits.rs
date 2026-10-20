// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the TCP active-flow limit.

use super::*;
use crate::limits::DEFAULT_MAX_ACTIVE_TCP_FLOWS;
use pal_async::task::Spawn;

/// Returns default parameters with the TCP active-flow limit set to `limit`.
fn params_with_tcp_limit(limit: Option<usize>) -> ConsommeParams {
    let mut params = ConsommeParams::new().unwrap();
    params.flow_limits.tcp = limit;
    params
}

/// Returns the four-tuple of a guest flow from `port` to a documentation
/// address.
fn guest_flow(port: u16) -> FourTuple {
    FourTuple {
        src: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), port)),
        dst: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 80)),
    }
}

/// Fills `tcp` with `count` active flows that hold no host socket.
fn insert_socketless_flows(tcp: &mut Tcp, count: usize) {
    for port in (10000..).take(count) {
        let conn = TcpConnection {
            backend: TcpBackend::Socket {
                socket: None,
                static_dns: None,
            },
            inner: TcpConnection::new_base(&tcp.connection_params),
        };
        tcp.connections.insert(guest_flow(port), conn);
    }
}

/// Inserts a flow from `port` in `state` that holds no host socket and whose
/// lifetime ends with `lifetime_timer`.
fn insert_flow_in_state(tcp: &mut Tcp, port: u16, state: TcpState, lifetime_timer: LifetimeTimer) {
    let mut inner = TcpConnection::new_base(&tcp.connection_params);
    inner.state = state;
    inner.lifetime_timer = lifetime_timer;
    let conn = TcpConnection {
        backend: TcpBackend::Socket {
            socket: None,
            static_dns: None,
        },
        inner,
    };
    tcp.connections.insert(guest_flow(port), conn);
}

/// Returns a guest SYN that opens a flow from `src_port` to `dst_port`.
fn guest_syn(src_port: u16, dst_port: u16) -> TcpRepr<'static> {
    TcpRepr {
        src_port,
        dst_port,
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
    }
}

#[cfg(unix)]
#[pal_async::async_test]
async fn test_tcp_port_forward_connection_limit(driver: DefaultDriver) {
    let mut harness =
        TcpTestHarness::connect_with_params(driver.clone(), params_with_tcp_limit(Some(1))).await;
    assert_eq!(harness.consomme.tcp.connections.len(), 1);

    let guest_port = 7777;
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let host_addr = socket.local_addr().unwrap().as_socket().unwrap();
    harness
        .consomme
        .access(&mut harness.client)
        .bind_tcp_port(socket, guest_port)
        .expect("bind should succeed");

    let mut connector = std::net::TcpStream::connect(host_addr).unwrap();
    connector.set_nonblocking(true).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        std::future::poll_fn(|cx| {
            harness.consomme.access(&mut harness.client).poll(cx);
            Poll::Ready(())
        })
        .await;

        let mut byte = [0];
        match io::Read::read(&mut connector, &mut byte) {
            Ok(0) => break,
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Ok(n) => panic!("unexpectedly read {n} bytes from rejected connection"),
            Err(err) => panic!("unexpected connector read error: {err}"),
        }

        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the over-limit accepted socket to close"
        );
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(Duration::from_millis(10))
            .await;
    }

    assert_eq!(
        harness.consomme.tcp.connections.len(),
        1,
        "an over-limit accepted socket must not grow the TCP flow map"
    );
}

#[pal_async::async_test]
async fn test_tcp_port_forward_repolls_after_limit_rejection(driver: DefaultDriver) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_addr = listener.local_addr().unwrap();
    let (connect_tx, connect_rx) = std::sync::mpsc::channel();
    // Connect only when signaled so the second connection arrives after the
    // first one has been rejected.
    let connector_thread = std::thread::spawn(move || {
        let mut connectors = Vec::new();
        for _ in 0..2 {
            connect_rx.recv().unwrap();
            connectors.push(std::net::TcpStream::connect(host_addr).unwrap());
        }
        connectors
    });

    let mut consomme = Consomme::new(params_with_tcp_limit(Some(0)));
    let mut client = TestClient::new(driver.clone());
    let guest_port = 7777;
    let key = PortForwardKey::new(IpVersion::Ipv4, guest_port);
    consomme
        .access(&mut client)
        .bind_tcp_port(Socket::from(listener), guest_port)
        .expect("bind should succeed");

    let task = driver.spawn("poll rejected TCP accepts", async move {
        let mut listener_armed = false;
        let mut rejected_first = false;
        std::future::poll_fn(move |cx| {
            if !listener_armed {
                // Avoid carrying an extra wake from immediately-ready IO into
                // the poll that rejects the first connection.
                assert!(
                    consomme
                        .tcp
                        .listeners
                        .get_mut(&key)
                        .unwrap()
                        .socket
                        .poll_ready(cx, PollEvents::IN)
                        .is_pending()
                );
                connect_tx.send(()).unwrap();
                listener_armed = true;
                return Poll::Pending;
            }

            consomme.access(&mut client).poll(cx);
            if !rejected_first {
                assert!(
                    consomme.tcp.connections.is_empty(),
                    "the zero flow limit should reject the first connection"
                );
                consomme.tcp.max_connections = Some(1);
                connect_tx.send(()).unwrap();
                rejected_first = true;
                return Poll::Pending;
            }

            if consomme.tcp.connections.len() == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    });

    let mut timer = pal_async::timer::PolledTimer::new(&driver);
    let timeout = timer.sleep(Duration::from_secs(5));
    let task = std::pin::pin!(task);
    let timeout = std::pin::pin!(timeout);
    let completed = !matches!(
        futures::future::select(task, timeout).await,
        futures::future::Either::Right(_)
    );
    connector_thread.join().unwrap();
    assert!(
        completed,
        "timed out waiting for a later inbound connection to be accepted"
    );
}

#[cfg(unix)]
#[pal_async::async_test]
async fn test_tcp_connection_limit_rejects_and_recovers(driver: DefaultDriver) {
    let mut harness =
        TcpTestHarness::connect_with_params(driver, params_with_tcp_limit(Some(1))).await;
    assert_eq!(harness.consomme.tcp.connections.len(), 1);

    let syn = guest_syn(harness.guest_port + 1, harness.dst_port);
    let rejected = harness.try_send_repr(&syn);
    assert!(matches!(rejected, Err(DropReason::TcpConnectionLimit)));
    assert_eq!(
        harness.consomme.tcp.connections.len(),
        1,
        "a rejected flow must not create a host TCP socket or buffer"
    );

    // Simulate the normal poll-time removal after a connection closes.
    harness.consomme.tcp.connections.clear();
    harness.try_send_repr(&syn).unwrap();
    assert_eq!(harness.consomme.tcp.connections.len(), 1);
}

#[test]
fn test_tcp_flow_limit_admission_beyond_default() {
    let new_flow = guest_flow(20000);

    let mut unbounded = Consomme::new(params_with_tcp_limit(None));
    insert_socketless_flows(&mut unbounded.tcp, DEFAULT_MAX_ACTIVE_TCP_FLOWS);
    assert!(
        unbounded.tcp.check_flow_limit(&new_flow).is_ok(),
        "an unbounded TCP flow map must admit a flow beyond the default limit"
    );

    let mut bounded = Consomme::new(ConsommeParams::new().unwrap());
    insert_socketless_flows(&mut bounded.tcp, DEFAULT_MAX_ACTIVE_TCP_FLOWS);
    assert!(matches!(
        bounded.tcp.check_flow_limit(&new_flow),
        Err(DropReason::TcpConnectionLimit)
    ));
    assert!(
        bounded.tcp.check_flow_limit(&guest_flow(10000)).is_ok(),
        "a full TCP flow map must keep admitting packets of its active flows"
    );
}

#[pal_async::async_test]
async fn test_tcp_flow_limit_applies_only_to_opening_syn(driver: DefaultDriver) {
    let mut consomme = Consomme::new(params_with_tcp_limit(Some(1)));
    insert_socketless_flows(&mut consomme.tcp, 1);
    let mut client = TestClient::new(driver);
    let params = consomme.params_mut();
    let (guest_mac, gateway_mac, guest_ip) =
        (params.client_mac, params.gateway_mac, params.client_ip);
    let dst_ip = Ipv4Address::new(192, 0, 2, 1);
    let mut buf = vec![0; 1514];

    // While the flow map is full, a segment of an unknown flow that cannot
    // open a connection still takes the usual path: one that acknowledges
    // something gets a reset, and any other one is ignored.
    let ack = TcpSeqNumber(4000);
    for (control, ack_number) in [
        (TcpControl::None, Some(ack)),
        (TcpControl::Psh, Some(ack)),
        (TcpControl::Fin, Some(ack)),
        (TcpControl::Syn, Some(ack)),
        (TcpControl::Rst, Some(ack)),
        (TcpControl::Rst, None),
        (TcpControl::Fin, None),
        (TcpControl::None, None),
    ] {
        let segment = TcpRepr {
            control,
            ack_number,
            window_scale: None,
            max_seg_size: None,
            ..guest_syn(20000, 80)
        };
        let len = build_tcp_packet(&mut buf, guest_mac, gateway_mac, guest_ip, dst_ip, &segment);
        if let Err(err) = consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE)
        {
            panic!("{control:?} segment acknowledging {ack_number:?} was dropped: {err}");
        }

        let replies = std::mem::take(&mut *client.received_packets.lock());
        if ack_number.is_some() && control != TcpControl::Rst {
            assert_eq!(replies.len(), 1, "{control:?} segment must get a reset");
            let (src_ip, reply_dst_ip, reply) = parse_tcp_packet(&replies[0]);
            assert_eq!((src_ip, reply_dst_ip), (dst_ip, guest_ip));
            assert_eq!((reply.src_port, reply.dst_port), (80, 20000));
            assert_eq!(reply.control, TcpControl::Rst);
            assert_eq!(reply.seq_number, ack);
            assert_eq!(reply.ack_number, None);
        } else {
            assert!(
                replies.is_empty(),
                "{control:?} segment acknowledging {ack_number:?} must be ignored"
            );
        }
        assert_eq!(consomme.tcp.connections.len(), 1);
    }

    // A SYN that would open a connection is still rejected before a host
    // socket is created for it.
    let len = build_tcp_packet(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        dst_ip,
        &guest_syn(20000, 80),
    );
    assert!(matches!(
        consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE),
        Err(DropReason::TcpConnectionLimit)
    ));
    assert!(client.received_packets.lock().is_empty());
    assert_eq!(consomme.tcp.connections.len(), 1);
    assert!(consomme.tcp.connections.contains_key(&guest_flow(10000)));
}

#[cfg(unix)]
#[pal_async::async_test]
async fn test_tcp_unbounded_flow_limit_admits_syn_beyond_default(driver: DefaultDriver) {
    let mut harness =
        TcpTestHarness::connect_with_params(driver, params_with_tcp_limit(None)).await;
    insert_socketless_flows(&mut harness.consomme.tcp, DEFAULT_MAX_ACTIVE_TCP_FLOWS - 1);
    assert_eq!(
        harness.consomme.tcp.connections.len(),
        DEFAULT_MAX_ACTIVE_TCP_FLOWS
    );

    harness
        .try_send_repr(&guest_syn(harness.guest_port + 1, harness.dst_port))
        .unwrap();
    assert_eq!(
        harness.consomme.tcp.connections.len(),
        DEFAULT_MAX_ACTIVE_TCP_FLOWS + 1,
        "an unbounded TCP flow map must admit a flow beyond the default limit"
    );
}

#[test]
fn test_tcp_flow_limit_reclaims_time_wait_closest_to_expiry() {
    let mut consomme = Consomme::new(params_with_tcp_limit(Some(3)));
    let tcp = &mut consomme.tcp;
    let now = Instant::now();
    insert_flow_in_state(tcp, 10000, TcpState::Established, LifetimeTimer::None);
    insert_flow_in_state(
        tcp,
        10001,
        TcpState::TimeWait,
        LifetimeTimer::Close(now + Duration::from_secs(30)),
    );
    insert_flow_in_state(
        tcp,
        10002,
        TcpState::TimeWait,
        LifetimeTimer::Close(now + Duration::from_secs(10)),
    );

    // Each new flow at the limit reclaims the TIME-WAIT connection closest to
    // expiry.
    for (new_port, reclaimed_port) in [(20000, 10002), (20001, 10001)] {
        tcp.check_flow_limit(&guest_flow(new_port)).unwrap();
        assert!(!tcp.connections.contains_key(&guest_flow(reclaimed_port)));
        insert_flow_in_state(tcp, new_port, TcpState::Established, LifetimeTimer::None);
    }
    assert_eq!(tcp.aggregate_stats.connections_reclaimed_time_wait.get(), 2);
    assert_eq!(tcp.aggregate_stats.connections_closed_normal.get(), 2);

    // Without a connection in TIME-WAIT, a new flow is rejected and no active
    // flow is evicted.
    assert!(matches!(
        tcp.check_flow_limit(&guest_flow(20002)),
        Err(DropReason::TcpConnectionLimit)
    ));
    assert_eq!(tcp.connections.len(), 3);

    // An unbounded map never cuts TIME-WAIT short.
    let mut unbounded = Consomme::new(params_with_tcp_limit(None));
    insert_flow_in_state(
        &mut unbounded.tcp,
        10000,
        TcpState::TimeWait,
        LifetimeTimer::Close(now + Duration::from_secs(10)),
    );
    unbounded.tcp.check_flow_limit(&guest_flow(20000)).unwrap();
    assert!(unbounded.tcp.connections.contains_key(&guest_flow(10000)));
    assert_eq!(
        unbounded
            .tcp
            .aggregate_stats
            .connections_reclaimed_time_wait
            .get(),
        0
    );
}

#[pal_async::async_test]
async fn test_tcp_flow_limit_reclaims_server_closed_time_wait(driver: DefaultDriver) {
    let mut h = TcpTestHarness::connect_with_params(driver, params_with_tcp_limit(Some(2))).await;
    insert_flow_in_state(
        &mut h.consomme.tcp,
        10000,
        TcpState::Established,
        LifetimeTimer::None,
    );

    // The server closes first, so Consomme closes actively toward the guest
    // and the connection lingers in TIME-WAIT once the guest closes too.
    h.clear_guest_packets();
    h.host_shutdown_write();
    let fin = h
        .poll_until_guest_packet(|tcp| tcp.control == TcpControl::Fin)
        .await;
    let (_, _, fin) = parse_tcp_packet(&fin);
    h.server_ack = fin.seq_number + fin.segment_len();
    h.send_segment(TcpControl::None, h.guest_seq, &[]);
    h.send_fin();
    assert_eq!(h.connection_inner().state, TcpState::TimeWait);

    // Until Consomme shuts down the host socket, the connection still has to
    // deliver the guest's FIN, so a new flow cannot reclaim it.
    let syn = guest_syn(h.guest_port + 1, h.dst_port);
    assert!(matches!(
        h.try_send_repr(&syn),
        Err(DropReason::TcpConnectionLimit)
    ));

    h.poll_until(|inner| inner.is_shutdown).await;
    h.try_send_repr(&syn).unwrap();
    assert!(!h.consomme.tcp.connections.contains_key(&h.four_tuple()));
    assert_eq!(h.consomme.tcp.connections.len(), 2);
    assert_eq!(
        h.consomme
            .tcp
            .aggregate_stats
            .connections_reclaimed_time_wait
            .get(),
        1
    );

    // With no connection left in TIME-WAIT, the next new flow is rejected.
    assert!(matches!(
        h.try_send_repr(&guest_syn(h.guest_port + 2, h.dst_port)),
        Err(DropReason::TcpConnectionLimit)
    ));
}

#[pal_async::async_test]
async fn test_tcp_port_forward_reclaims_time_wait(driver: DefaultDriver) {
    let mut h =
        TcpTestHarness::connect_with_params(driver.clone(), params_with_tcp_limit(Some(2))).await;
    insert_flow_in_state(
        &mut h.consomme.tcp,
        10000,
        TcpState::TimeWait,
        LifetimeTimer::Close(Instant::now() + Duration::from_secs(60)),
    );

    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let host_addr = socket.local_addr().unwrap().as_socket().unwrap();
    h.consomme
        .access(&mut h.client)
        .bind_tcp_port(socket, 7777)
        .expect("bind should succeed");
    let _connector = std::net::TcpStream::connect(host_addr).unwrap();

    // The accepted connection takes the slot of the TIME-WAIT connection.
    let time_wait_flow = guest_flow(10000);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while h.consomme.tcp.connections.contains_key(&time_wait_flow) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the accepted connection to reclaim the TIME-WAIT slot"
        );
        std::future::poll_fn(|cx| {
            h.consomme.access(&mut h.client).poll(cx);
            Poll::Ready(())
        })
        .await;
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(Duration::from_millis(10))
            .await;
    }
    assert_eq!(h.consomme.tcp.connections.len(), 2);
    assert_eq!(
        h.consomme
            .tcp
            .aggregate_stats
            .connections_reclaimed_time_wait
            .get(),
        1
    );
}
