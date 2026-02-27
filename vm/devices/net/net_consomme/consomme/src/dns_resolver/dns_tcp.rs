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
use super::DnsResolver;
use super::DnsResponse;
use mesh_channel_core::Receiver;
use std::io::IoSliceMut;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use thiserror::Error;

// Maximum allowed DNS message size over TCP: 65535 bytes for the message
// plus 2 bytes for the TCP length prefix. This is a sanity check to prevent
// unbounded memory growth.
const MAX_DNS_TCP_PAYLOAD_SIZE: usize = (u16::MAX as usize) + 2;

/// Errors returned by [`DnsTcpHandler::ingest`] and [`DnsTcpHandler::poll_read`]
/// when the DNS TCP framing is invalid or the query cannot be processed.
#[derive(Debug, Error)]
pub enum DnsTcpError {
    /// The TCP length prefix specified a message size too small for a valid DNS header.
    #[error("invalid DNS TCP message length")]
    InvalidMessageLength,
    /// The query was rate-limited by the resolver backend.
    #[error("DNS TCP query rate-limited")]
    RateLimited,
}

/// Current phase of the DNS TCP handler state machine.
enum Phase {
    /// Accumulating an incoming TCP-framed DNS request.
    Receiving,
    /// Query submitted to the backend; awaiting response.
    InFlight,
    /// Writing a TCP-framed response back to the caller.
    Responding,
}

pub struct DnsTcpHandler {
    receiver: Receiver<DnsResponse>,
    flow: DnsFlow,
    /// Shared buffer used for both the incoming request and the outgoing
    /// response.  During [`Phase::Receiving`] it accumulates one TCP-framed
    /// DNS message from the guest.  During [`Phase::Responding`] it holds
    /// the TCP-framed response being drained to the caller.
    buf: Vec<u8>,
    /// Write offset into `buf` while draining a response to the caller.
    /// Only meaningful during [`Phase::Responding`].
    tx_offset: usize,
    phase: Phase,
    /// The guest has sent FIN; no more data will arrive.
    guest_fin: bool,
}

impl DnsTcpHandler {
    pub fn new(flow: DnsFlow) -> Self {
        let receiver = Receiver::new();
        Self {
            receiver,
            flow,
            buf: Vec::new(),
            tx_offset: 0,
            phase: Phase::Receiving,
            guest_fin: false,
        }
    }

    /// Feed data received from the guest into the handler.
    ///
    /// Consumes bytes from `data` to assemble one complete TCP-framed DNS
    /// message. When a complete message is assembled, it is submitted to the
    /// backend for resolution and no further data is accepted until the
    /// response has been fully written out by [`poll_read`].
    ///
    /// Returns the number of bytes consumed from `data`. The caller should
    /// only drain this many bytes from its receive buffer.
    ///
    /// Returns an error if the TCP framing is invalid or the query cannot be
    /// submitted, in which case the caller should reset the connection.
    pub fn ingest<B: DnsBackend>(
        &mut self,
        data: &[&[u8]],
        dns: &mut DnsResolver<B>,
    ) -> Result<usize, DnsTcpError> {
        // Don't accept data while a query is in-flight or a response is pending.
        if !matches!(self.phase, Phase::Receiving) {
            return Ok(0);
        }

        let mut total_consumed = 0;
        for chunk in data {
            let mut pos = 0;
            while pos < chunk.len() {
                let need = self.bytes_needed();
                if need == 0 {
                    // Complete message already in rx_buf but not yet submitted
                    // (should not happen in practice).
                    break;
                }
                let accept = (chunk.len() - pos).min(need);
                self.buf.extend_from_slice(&chunk[pos..pos + accept]);
                pos += accept;
                total_consumed += accept;

                match self.try_submit(dns) {
                    Ok(true) => return Ok(total_consumed),
                    Ok(false) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        tracing::info!(
            total_consumed,
            buf_len = self.buf.len(),
            "dns_tcp ingest: done (message incomplete)"
        );
        Ok(total_consumed)
    }

    /// How many more bytes are needed to complete the current message.
    fn bytes_needed(&self) -> usize {
        if self.buf.len() < 2 {
            return 2 - self.buf.len();
        }
        let msg_len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
        (2 + msg_len).saturating_sub(self.buf.len())
    }

    /// If a complete TCP-framed DNS message is in `buf`, submit it to the
    /// resolver via [`DnsResolver::submit_tcp_query`].
    ///
    /// Returns `Ok(true)` if the query was submitted, `Ok(false)` if the
    /// message is still incomplete, or `Err` if the framing is invalid or
    /// the query was rejected.
    fn try_submit<B: DnsBackend>(&mut self, dns: &mut DnsResolver<B>) -> Result<bool, DnsTcpError> {
        if self.buf.len() < 2 {
            return Ok(false);
        }
        let msg_len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
        if msg_len <= super::DNS_HEADER_SIZE {
            return Err(DnsTcpError::InvalidMessageLength);
        }
        if self.buf.len() < 2 + msg_len {
            return Ok(false);
        }

        // Submit the raw DNS query (without the TCP length prefix).
        let request = DnsRequest {
            flow: self.flow.clone(),
            dns_query: &self.buf[2..2 + msg_len],
        };
        if !dns.submit_tcp_query(&request, self.receiver.sender()) {
            tracelimit::warn_ratelimited!(
                msg_len,
                src_port = self.flow.src_port,
                "dns_tcp: query rate-limited, closing connection"
            );
            return Err(DnsTcpError::RateLimited);
        }
        self.buf.clear();
        self.phase = Phase::InFlight;
        Ok(true)
    }

    /// Poll for the next chunk of response data.
    ///
    /// Models the socket `poll_read_vectored` contract:
    /// - `Poll::Ready(Ok(n))` where `n > 0`: wrote `n` bytes of response data.
    /// - `Poll::Ready(Ok(0))`: EOF — the guest sent FIN and all responses have
    ///   been drained. The caller should close the connection.
    /// - `Poll::Ready(Err(_))`: a protocol error occurred; the caller should
    ///   reset the connection.
    /// - `Poll::Pending`: waiting for a DNS response or for [`ingest`] to
    ///   submit a new query.
    pub fn poll_read<B: DnsBackend>(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        dns: &mut DnsResolver<B>,
    ) -> Poll<Result<usize, DnsTcpError>> {
        match self.phase {
            Phase::InFlight => {
                match ready!(self.receiver.poll_recv(cx)) {
                    Ok(response) => {
                        dns.complete_tcp_query();
                        let payload_len = response.response_data.len();
                        if payload_len > MAX_DNS_TCP_PAYLOAD_SIZE {
                            tracelimit::warn_ratelimited!(
                                size = payload_len,
                                "DNS TCP response exceeds maximum message size, dropping"
                            );
                            // Discard the oversized response and return to
                            // receiving so that ingest can accept the next query.
                            self.phase = Phase::Receiving;
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }

                        // Build TCP-framed response: 2-byte length prefix + payload.
                        self.buf.clear();
                        self.buf
                            .reserve((2 + payload_len).saturating_sub(self.buf.capacity()));
                        self.buf
                            .extend_from_slice(&(payload_len as u16).to_be_bytes());
                        self.buf.extend(response.response_data);
                        self.tx_offset = 0;
                        self.phase = Phase::Responding;

                        let n = self.drain_tx(bufs);
                        return Poll::Ready(Ok(n));
                    }
                    Err(_) => {
                        self.phase = Phase::Receiving;
                    }
                }
            }
            Phase::Responding => {
                let n = self.drain_tx(bufs);
                return Poll::Ready(Ok(n));
            }
            Phase::Receiving => {}
        }

        // No in-flight query and no pending response.
        if self.guest_fin {
            Poll::Ready(Ok(0))
        } else {
            Poll::Pending
        }
    }

    /// Write as much of `buf[tx_offset..]` into `bufs` as possible.
    /// Clears `buf` when fully drained so it can be reused for the next
    /// incoming request.
    fn drain_tx(&mut self, bufs: &mut [IoSliceMut<'_>]) -> usize {
        let remaining = &self.buf[self.tx_offset..];
        let mut written = 0;
        for buf in bufs.iter_mut() {
            let left = remaining.len() - written;
            if left == 0 {
                break;
            }
            let n = buf.len().min(left);
            buf[..n].copy_from_slice(&remaining[written..written + n]);
            written += n;
        }
        self.tx_offset += written;
        if self.tx_offset >= self.buf.len() {
            self.buf.clear();
            self.tx_offset = 0;
            self.phase = Phase::Receiving;
        }
        written
    }

    pub fn guest_fin(&self) -> bool {
        self.guest_fin
    }

    pub fn set_guest_fin(&mut self) {
        self.guest_fin = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns_resolver::DnsBackend;
    use crate::dns_resolver::DnsRequest;
    use crate::dns_resolver::DnsResponse;
    use std::sync::Arc;

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

    /// A 16-byte fake DNS query payload (>= 12-byte header minimum).
    fn sample_query() -> Vec<u8> {
        vec![
            0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x66,
            0x6F, 0x6F,
        ]
    }

    struct NoopWaker;
    impl std::task::Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn single_query_response() {
        let mut dns = DnsResolver::new_for_test(Arc::new(EchoBackend));
        let mut handler = DnsTcpHandler::new(test_flow());

        let query = sample_query();
        let msg = make_tcp_dns_message(&query);

        let consumed = handler.ingest(&[&msg], &mut dns).unwrap();
        assert_eq!(consumed, msg.len());

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let mut buf = vec![0u8; 256];
        match handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns) {
            Poll::Ready(Ok(n)) => {
                assert!(n > 0);
                // First 2 bytes are the TCP length prefix.
                let resp_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                assert_eq!(resp_len, query.len());
                // Response payload should match the query (echo backend).
                assert_eq!(&buf[2..2 + resp_len], &query);
            }
            Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
            Poll::Pending => panic!("expected Ready"),
        }
    }

    #[test]
    fn partial_message_buffering() {
        let mut dns = DnsResolver::new_for_test(Arc::new(EchoBackend));
        let mut handler = DnsTcpHandler::new(test_flow());

        let query = sample_query();
        let msg = make_tcp_dns_message(&query);

        // Feed just the length prefix.
        let consumed = handler.ingest(&[&msg[..2]], &mut dns).unwrap();
        assert_eq!(consumed, 2);

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        let mut buf = vec![0u8; 256];
        assert!(matches!(
            handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns),
            Poll::Pending
        ));

        // Feed the rest.
        let consumed = handler.ingest(&[&msg[2..]], &mut dns).unwrap();
        assert_eq!(consumed, msg.len() - 2);

        match handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns) {
            Poll::Ready(Ok(n)) => assert!(n > 0),
            Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
            Poll::Pending => panic!("expected Ready after completing message"),
        }
    }

    #[test]
    fn backpressure_one_at_a_time() {
        let mut dns = DnsResolver::new_for_test(Arc::new(EchoBackend));
        let mut handler = DnsTcpHandler::new(test_flow());

        let q1 = sample_query();
        let q2 = vec![
            0x00, 0x02, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x62,
            0x62, 0x62,
        ];
        let mut combined = make_tcp_dns_message(&q1);
        combined.extend(make_tcp_dns_message(&q2));

        // Only the first message should be consumed.
        let consumed = handler.ingest(&[&combined], &mut dns).unwrap();
        assert_eq!(consumed, make_tcp_dns_message(&q1).len());

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        // Drain the first response.
        let mut buf = vec![0u8; 256];
        match handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns) {
            Poll::Ready(Ok(n)) => assert!(n > 0),
            Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
            Poll::Pending => panic!("expected Ready for first response"),
        }

        // Now the second message can be ingested.
        let remaining = &combined[consumed..];
        let consumed2 = handler.ingest(&[remaining], &mut dns).unwrap();
        assert_eq!(consumed2, make_tcp_dns_message(&q2).len());

        match handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns) {
            Poll::Ready(Ok(n)) => assert!(n > 0),
            Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
            Poll::Pending => panic!("expected Ready for second response"),
        }
    }

    #[test]
    fn eof_after_fin_and_drain() {
        let mut dns = DnsResolver::new_for_test(Arc::new(EchoBackend));
        let mut handler = DnsTcpHandler::new(test_flow());

        let query = sample_query();
        handler
            .ingest(&[&make_tcp_dns_message(&query)], &mut dns)
            .unwrap();

        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        // Drain the response.
        let mut buf = vec![0u8; 256];
        let _ = handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns);

        handler.set_guest_fin();

        // Should now report EOF.
        assert!(matches!(
            handler.poll_read(&mut cx, &mut [IoSliceMut::new(&mut buf)], &mut dns),
            Poll::Ready(Ok(0))
        ));
    }

    #[test]
    fn protocol_error_on_invalid_length() {
        let mut dns = DnsResolver::new_for_test(Arc::new(EchoBackend));
        let mut handler = DnsTcpHandler::new(test_flow());

        // Craft a message with msg_len <= DNS_HEADER_SIZE (12).
        // Length prefix says 4 bytes, which is too small for a DNS header.
        let bad_msg = [0x00, 0x04, 0x01, 0x02, 0x03, 0x04];
        assert!(matches!(
            handler.ingest(&[&bad_msg], &mut dns),
            Err(DnsTcpError::InvalidMessageLength)
        ));
    }
}
