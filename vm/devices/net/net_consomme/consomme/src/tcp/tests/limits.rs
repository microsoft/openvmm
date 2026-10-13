// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the TCP active-flow limit.

use super::*;

#[pal_async::async_test]
async fn test_tcp_port_forward_connection_limit(driver: DefaultDriver) {
    let mut harness = TcpTestHarness::connect(driver.clone()).await;
    harness.consomme.tcp.max_connections = 1;
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
    let mut harness = TcpTestHarness::connect(driver).await;
    harness.consomme.tcp.max_connections = 1;
    assert_eq!(harness.consomme.tcp.connections.len(), 1);

    let syn = TcpRepr {
        src_port: harness.guest_port + 1,
        dst_port: harness.dst_port,
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
    let len = build_tcp_packet(
        &mut harness.buf,
        harness.guest_mac,
        harness.gateway_mac,
        harness.guest_ip,
        harness.dst_ip,
        &syn,
    );
    let rejected = harness
        .consomme
        .access(&mut harness.client)
        .send(&harness.buf[..len], &ChecksumState::NONE);
    assert!(matches!(rejected, Err(DropReason::TcpConnectionLimit)));
    assert_eq!(
        harness.consomme.tcp.connections.len(),
        1,
        "a rejected flow must not create a host TCP socket or buffer"
    );

    // Simulate the normal poll-time removal after a connection closes.
    harness.consomme.tcp.connections.clear();
    harness
        .consomme
        .access(&mut harness.client)
        .send(&harness.buf[..len], &ChecksumState::NONE)
        .unwrap();
    assert_eq!(harness.consomme.tcp.connections.len(), 1);
}
