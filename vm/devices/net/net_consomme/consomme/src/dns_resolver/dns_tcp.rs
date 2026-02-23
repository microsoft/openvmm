// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS over TCP handler for consomme.
//!
//! Implements DNS TCP framing per RFC 1035 §4.2.2: each DNS message is
//! preceded by a 2-byte big-endian length prefix. This module intercepts
//! TCP connections to the gateway on port 53 and resolves queries using
//! the shared `DnsBackend`.

use super::DnsBackend;
use super::DnsFlow;
use super::DnsRequest;
use super::DnsResponse;
use mesh_channel_core::Receiver;
use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// There is no official maximum size for DNS messages over TCP, but we can set
/// a reasonable upper bound to u16::MAX (65535 bytes) to prevent unbounded memory 
/// usage. This is larger than the typical 512-byte limit for UDP, as TCP can 
/// handle larger messages.
const MAX_DNS_TCP_MESSAGE_SIZE: usize = 65535;

pub struct DnsTcpHandler {
    backend: Arc<dyn DnsBackend>,
    receiver: Receiver<DnsResponse>,
    flow: DnsFlow,
    /// Data received from the guest, accumulating DNS TCP framed messages.
    rx_buf: VecDeque<u8>,
    /// Length-prefixed DNS responses waiting to be sent to the guest.
    tx_buf: VecDeque<u8>,
    /// The guest has sent FIN; no more data will arrive.
    guest_fin: bool,
    /// Number of DNS queries submitted but not yet resolved.
    in_flight: usize,
}

impl DnsTcpHandler {
    pub fn new(backend: Arc<dyn DnsBackend>, flow: DnsFlow) -> Self {
        let receiver = Receiver::new();
        Self {
            backend,
            receiver,
            flow,
            rx_buf: VecDeque::new(),
            tx_buf: VecDeque::new(),
            guest_fin: false,
            in_flight: 0,
        }
    }

    /// Feed data received from the guest into the handler.
    /// Extracts complete DNS messages and submits them for resolution.
    pub fn ingest(&mut self, data: &[&[u8]]) {
        for chunk in data {
            let remaining_capacity = MAX_DNS_TCP_MESSAGE_SIZE.saturating_sub(self.rx_buf.len());
            let accepted = chunk.len().min(remaining_capacity);
            if accepted > 0 {
                self.rx_buf.extend(&chunk[..accepted]);
            }
            if accepted < chunk.len() {
                tracelimit::warn_ratelimited!(
                    dropped = chunk.len() - accepted,
                    "DNS TCP rx_buf full, dropping excess data"
                );
            }
        }
        self.extract_and_submit_queries();
    }

    /// Parse the rx buffer for complete DNS TCP-framed messages
    /// (2-byte big-endian length prefix + payload) and submit each query.
    fn extract_and_submit_queries(&mut self) {
        loop {
            if self.rx_buf.len() < 2 {
                break;
            }
            let msg_len = u16::from_be_bytes([self.rx_buf[0], self.rx_buf[1]]) as usize;
            if msg_len == 0 || msg_len > MAX_DNS_TCP_MESSAGE_SIZE {
                // Malformed: discard the length prefix and try to resync.
                self.rx_buf.drain(..2);
                continue;
            }
            if self.rx_buf.len() < 2 + msg_len {
                // Incomplete message; wait for more data.
                break;
            }
            // On Windows, the two byte prefix must be included in the buffer
            // sent to the backend, as DnsQueryRaw expects the full TCP-framed
            // message.
            // On Unix, the backend expects just the raw DNS query without the TCP prefix,
            // so we strip it before sending.
            #[cfg(unix)]
            let query_data = {
                self.rx_buf.drain(..2);
                self.rx_buf.drain(..msg_len).collect::<Vec<u8>>()
            };
            #[cfg(windows)]
            let query_data = self.rx_buf.drain(..2 + msg_len).collect::<Vec<u8>>();

            let request = DnsRequest {
                flow: self.flow.clone(),
                dns_query: &query_data,
            };
            self.backend.query(&request, self.receiver.sender());
            self.in_flight += 1;
        }
    }

    /// Poll for completed DNS responses and write length-prefixed data
    /// Returns the total number of bytes written.
    pub fn poll_read(&mut self, cx: &mut Context<'_>, bufs: &mut [IoSliceMut<'_>]) -> usize {
        while let Poll::Ready(Ok(response)) = self.receiver.poll_recv(cx) {
            self.in_flight = self.in_flight.saturating_sub(1);
            if response.response_data.len() > MAX_DNS_TCP_MESSAGE_SIZE {
                tracelimit::warn_ratelimited!(
                    size = response.response_data.len(),
                    "DNS TCP response exceeds maximum message size, dropping"
                );
                continue;
            }
            #[cfg(unix)]
            {
                let len = response.response_data.len() as u16;
                self.tx_buf.extend(&len.to_be_bytes());
            }
            self.tx_buf.extend(&response.response_data);
        }
        self.drain_buffered(bufs)
    }

    /// Drain buffered tx data into the provided buffers.
    fn drain_buffered(&mut self, bufs: &mut [IoSliceMut<'_>]) -> usize {
        let mut total = 0;
        for buf in bufs.iter_mut() {
            if self.tx_buf.is_empty() {
                break;
            }
            let n = buf.len().min(self.tx_buf.len());
            for (dst, src) in buf[..n].iter_mut().zip(self.tx_buf.drain(..n)) {
                *dst = src;
            }
            total += n;
        }
        total
    }

    pub fn has_pending_tx(&self) -> bool {
        !self.tx_buf.is_empty()
    }

    pub fn guest_fin(&self) -> bool {
        self.guest_fin
    }

    pub fn set_guest_fin(&mut self) {
        self.guest_fin = true;
    }

    /// Returns true when the guest has sent FIN, all in-flight queries
    /// have been resolved, and all responses have been flushed.
    pub fn should_close(&self) -> bool {
        self.guest_fin && self.in_flight == 0 && self.tx_buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test DNS backend that echoes the query back as the response.
    struct EchoBackend;

    impl DnsBackend for EchoBackend {
        fn query(
            &self,
            request: &DnsRequest<'_>,
            response_sender: mesh_channel_core::Sender<DnsResponse>,
        ) {
            response_sender.send(DnsResponse {
                flow: request.flow.clone(),
                response_data: request.dns_query.to_vec(),
            });
        }
    }

    fn test_flow() -> DnsFlow {
        use smoltcp::wire::EthernetAddress;
        use smoltcp::wire::IpAddress;
        use smoltcp::wire::Ipv4Address;
        DnsFlow {
            src_addr: IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)),
            dst_addr: IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1)),
            src_port: 12345,
            dst_port: 53,
            gateway_mac: EthernetAddress([0x52, 0x55, 10, 0, 0, 1]),
            client_mac: EthernetAddress([0, 0, 0, 0, 1, 0]),
            transport: crate::dns_resolver::DnsTransport::Tcp,
        }
    }

    fn make_tcp_dns_message(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u16;
        let mut msg = len.to_be_bytes().to_vec();
        msg.extend_from_slice(payload);
        msg
    }

    #[test]
    fn single_query_response() {
        let backend = Arc::new(EchoBackend);
        let mut handler = DnsTcpHandler::new(backend, test_flow());

        // 22-byte fake DNS query (> 12-byte header minimum)
        let query = vec![
            0x00, 0x14, 0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x03, 0x77, 0x77, 0x77, 0x03, 0x63, 0x6F, 0x6D,
        ];
        let msg = make_tcp_dns_message(&query);

        handler.ingest(&[&msg]);

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let mut buf = vec![0u8; 256];
        handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)]);
        assert_eq!(u16::from_be_bytes([buf[0], buf[1]]) as usize, query.len());
    }

    #[test]
    fn partial_message_buffering() {
        let backend = Arc::new(EchoBackend);
        let mut handler = DnsTcpHandler::new(backend, test_flow());

        let query = vec![
            0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x66,
            0x6F, 0x6F,
        ];
        let msg = make_tcp_dns_message(&query);

        // Feed just the length prefix
        handler.ingest(&[&msg[..2]]);

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        let mut buf = vec![0u8; 256];
        assert_eq!(
            handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)]),
            0
        );

        // Feed the rest
        handler.ingest(&[&msg[2..]]);
        assert!(handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)]) > 0);
    }

    #[test]
    fn multiple_queries_in_one_write() {
        let backend = Arc::new(EchoBackend);
        let mut handler = DnsTcpHandler::new(backend, test_flow());

        let q1 = vec![
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x61,
            0x61, 0x61,
        ];
        let q2 = vec![
            0x00, 0x02, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x62,
            0x62, 0x62,
        ];
        let mut combined = make_tcp_dns_message(&q1);
        combined.extend(make_tcp_dns_message(&q2));

        handler.ingest(&[&combined]);

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let mut buf = vec![0u8; 512];
        let n = handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)]);
        // Each response is a 2-byte TCP length prefix + the DNS payload.
        let per_response = q1.len() + 2; // 2-byte TCP prefix + DNS payload
        assert_eq!(n, 2 * per_response);
    }

    #[test]
    fn should_close_after_fin_and_drain() {
        let backend = Arc::new(EchoBackend);
        let mut handler = DnsTcpHandler::new(backend, test_flow());

        let query = vec![
            0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x78,
            0x78, 0x78,
        ];
        handler.ingest(&[&make_tcp_dns_message(&query)]);
        handler.set_guest_fin();

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let mut buf = vec![0u8; 256];
        let _ = handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)]);

        // tx_buf is now drained, but we need to verify should_close
        // only returns true after all data is consumed.
        assert!(!handler.has_pending_tx());

        assert!(handler.should_close());
    }

    struct NoopWaker;
    impl std::task::Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }
}
