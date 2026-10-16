// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the TCP active-flow limit.

use super::*;
use crate::limits::DEFAULT_MAX_ACTIVE_TCP_FLOWS;

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

/// Returns a guest SYN that opens a flow from `src_port` to `dst_port`.
#[cfg(unix)]
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
