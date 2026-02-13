// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;
use mesh_channel_core::Receiver;
use mesh_channel_core::Sender;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpAddress;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use crate::DropReason;

pub mod dns_tcp;

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

static DNS_HEADER_SIZE: usize = 12;

/// Transport protocol for a DNS query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsTransport {
    Udp,
    Tcp,
}

#[derive(Debug, Clone)]
pub struct DnsFlow {
    pub src_addr: IpAddress,
    pub dst_addr: IpAddress,
    pub src_port: u16,
    pub dst_port: u16,
    pub gateway_mac: EthernetAddress,
    pub client_mac: EthernetAddress,
    // Used by the glibc and Windows DNS backends, but not the musl backend.
    #[allow(dead_code)]
    pub transport: DnsTransport,
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

pub(crate) trait DnsBackend: Send + Sync {
    fn query(&self, request: &DnsRequest<'_>, response_sender: Sender<DnsResponse>);
}

#[derive(Inspect)]
pub struct DnsResolver {
    #[inspect(skip)]
    backend: Arc<dyn DnsBackend>,
    #[inspect(skip)]
    receiver: Receiver<DnsResponse>,
    pending_requests: usize,
    max_pending_requests: usize,
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

        let receiver = Receiver::new();
        Ok(Self {
            backend: Arc::new(WindowsDnsResolverBackend::new()?),
            receiver,
            pending_requests: 0,
            max_pending_requests,
        })
    }

    /// Creates a new DNS resolver with a configurable limit on pending requests.
    ///
    /// # Arguments
    /// * `max_pending_requests` - Maximum number of concurrent pending DNS requests.
    #[cfg(unix)]
    pub fn new(max_pending_requests: usize) -> Result<Self, std::io::Error> {
        use crate::dns_resolver::unix::UnixDnsResolverBackend;

        let receiver = Receiver::new();
        Ok(Self {
            backend: Arc::new(UnixDnsResolverBackend::new()?),
            receiver,
            pending_requests: 0,
            max_pending_requests,
        })
    }

    pub fn handle_dns(&mut self, request: &DnsRequest<'_>) -> Result<(), DropReason> {
        if request.dns_query.len() <= DNS_HEADER_SIZE {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        if self.pending_requests < self.max_pending_requests {
            self.pending_requests += 1;
            self.backend.query(request, self.receiver.sender());
        } else {
            tracelimit::warn_ratelimited!(
                current = self.pending_requests,
                max = self.max_pending_requests,
                "DNS request limit reached"
            );
        }

        Ok(())
    }

    pub fn poll_response(&mut self, cx: &mut Context<'_>) -> Poll<Option<DnsResponse>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Ok(response)) => {
                self.pending_requests -= 1;
                Poll::Ready(Some(response))
            }
            Poll::Ready(Err(_)) | Poll::Pending => Poll::Pending,
        }
    }

    pub fn backend(&self) -> &Arc<dyn DnsBackend> {
        &self.backend
    }
}

/// Internal DNS request structure used by backend implementations.
#[derive(Debug)]
pub(crate) struct DnsRequestInternal {
    pub flow: DnsFlow,
    pub query: Vec<u8>,
    pub response_sender: Sender<DnsResponse>,
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
