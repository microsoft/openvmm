// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh_channel_core::Receiver;
use mesh_channel_core::Sender;
use mesh_channel_core::channel;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::Ipv4Address;
use std::task::Context;
use std::task::Poll;

use crate::DropReason;

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

static DNS_HEADER_SIZE: usize = 12;

#[derive(Debug, Clone)]
pub struct DnsFlow {
    pub src_addr: Ipv4Address,
    pub dst_addr: Ipv4Address,
    pub src_port: u16,
    pub dst_port: u16,
    pub gateway_mac: EthernetAddress,
    pub client_mac: EthernetAddress,
}

#[derive(Debug, Clone)]
pub struct DnsRequest<'a> {
    pub flow: DnsFlow,
    pub dns_query: &'a [u8],
}

/// A queued DNS response ready to be sent to the guest.
#[derive(Debug, Clone)]
pub struct DnsResponse {
    pub flow: DnsFlow,
    pub response_data: Vec<u8>,
}

/// Thread-safe accessor that allows backends to enqueue responses without
/// borrowing `DnsResolver` (e.g., from a background thread).
#[derive(Debug, Clone)]
pub struct DnsResponseAccessor {
    sender: Sender<DnsResponse>,
}

impl DnsResponseAccessor {
    pub fn push(&self, response: DnsResponse) {
        self.sender.send(response);
    }
}

pub trait DnsBackend: Send + Sync {
    fn query(
        &self,
        request: &DnsRequest<'_>,
        accessor: DnsResponseAccessor,
    ) -> Result<(), DropReason>;

    fn cancel_all(&mut self) -> Result<(), std::io::Error>;
}

pub struct DnsResolver {
    backend: Box<dyn DnsBackend>,
    sender: Sender<DnsResponse>,
    receiver: Receiver<DnsResponse>,
}

/// Default maximum number of pending DNS requests.
pub const DEFAULT_MAX_PENDING_DNS_REQUESTS: usize = 256;

impl DnsResolver {
    /// Creates a new DNS resolver with a configurable limit on pending requests.
    ///
    /// # Arguments
    /// * `max_pending_requests` - Maximum number of concurrent pending DNS requests.
    #[cfg(windows)]
    pub fn new(max_pending_requests: usize) -> Result<Self, std::io::Error> {
        use crate::dns_resolver::windows::WindowsDnsResolverBackend;

        let (sender, receiver) = channel();
        Ok(Self {
            backend: Box::new(WindowsDnsResolverBackend::new(max_pending_requests)?),
            sender,
            receiver,
        })
    }

    /// Creates a new DNS resolver with a configurable limit on pending requests.
    ///
    /// # Arguments
    /// * `max_pending_requests` - Maximum number of concurrent pending DNS requests.
    ///
    /// Note: On Unix platforms, this parameter is currently ignored as the backend
    /// does not yet support limiting pending requests.
    #[cfg(unix)]
    pub fn new(_max_pending_requests: usize) -> Result<Self, std::io::Error> {
        use crate::dns_resolver::unix::UnixDnsResolverBackend;

        let (sender, receiver) = channel();
        Ok(Self {
            backend: Box::new(UnixDnsResolverBackend::new()?),
            sender,
            receiver,
        })
    }

    pub fn handle_dns(&mut self, request: &DnsRequest<'_>) -> Result<(), DropReason> {
        if request.dns_query.len() <= 12 {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        let accessor = DnsResponseAccessor {
            sender: self.sender.clone(),
        };

        self.backend.query(request, accessor)
    }

    pub fn poll_responses(&mut self, cx: &mut Context<'_>) -> Vec<DnsResponse> {
        let mut responses = Vec::new();
        loop {
            match self.receiver.poll_recv(cx) {
                Poll::Ready(Ok(response)) => responses.push(response),
                Poll::Ready(Err(_)) => break, // Channel closed
                Poll::Pending => break,
            }
        }
        responses
    }

    pub fn cancel_all(&mut self) -> Result<(), std::io::Error> {
        self.backend.cancel_all()
    }
}

impl Drop for DnsResolver {
    fn drop(&mut self) {
        let _ = self.cancel_all();
    }
}

/// Internal DNS request structure used by backend implementations.
#[derive(Debug)]
pub(crate) struct DnsRequestInternal {
    pub flow: DnsFlow,
    pub query: Vec<u8>,
    pub accessor: DnsResponseAccessor,
}

pub(crate) fn build_servfail_response(query: &[u8]) -> Vec<u8> {
    // We need at least the DNS header (12 bytes) to build a response
    if query.len() < DNS_HEADER_SIZE {
        // Return an empty response if the query is malformed
        return Vec::new();
    }

    let mut response = Vec::with_capacity(query.len());

    // Copy transaction ID from query (bytes 0-1)
    response.extend_from_slice(&query[0..2]);

    // Build flags: QR=1 (response), OPCODE=0, AA=0, TC=0, RD=query.RD, RA=1, RCODE=2 (SERVFAIL)
    let rd = query[2] & 0x01; // Preserve RD bit from query
    let flags_byte1 = 0x80 | rd; // QR=1, RD preserved
    let flags_byte2 = 0x82; // RA=1, RCODE=2 (SERVFAIL)
    response.push(flags_byte1);
    response.push(flags_byte2);

    // Copy QDCOUNT from query (bytes 4-5)
    response.extend_from_slice(&query[4..6]);

    // ANCOUNT = 0, NSCOUNT = 0, ARCOUNT = 0
    response.extend_from_slice(&[0, 0, 0, 0, 0, 0]);

    // Copy the question section if present
    if query.len() > DNS_HEADER_SIZE {
        response.extend_from_slice(&query[DNS_HEADER_SIZE..]);
    }

    response
}
