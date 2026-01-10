// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use parking_lot::Mutex;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::sync::Arc;
use std::task::Context;
use std::task::Waker;

use crate::DropReason;

#[cfg(unix)]
#[path = "dns_resolver_unix.rs"]
mod resolver;

#[cfg(windows)]
#[path = "dns_resolver_windows.rs"]
mod resolver_raw;

#[cfg(windows)]
mod delay_load;

static DNS_HEADER_SIZE: usize = 12;

#[derive(Debug, Clone)]
pub struct DnsFlow {
    pub src_addr: Ipv4Address,
    pub dst_addr: Ipv4Address,
    pub src_port: u16,
    pub dst_port: u16,
    pub gateway_mac: EthernetAddress,
    pub client_mac: EthernetAddress,
    pub protocol: IpProtocol,
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

struct DnsResponseQueues {
    udp: Mutex<Vec<DnsResponse>>,
    tcp: Mutex<Vec<DnsResponse>>,
    waker: Mutex<Option<Waker>>,
}

// Manual Debug implementation since Waker doesn't implement Debug
impl std::fmt::Debug for DnsResponseQueues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsResponseQueues")
            .field("udp", &self.udp)
            .field("tcp", &self.tcp)
            .field("waker", &self.waker.lock().is_some())
            .finish()
    }
}

/// Thread-safe accessor that allows backends to enqueue responses without
/// borrowing `DnsResolver` (e.g., from a background thread).
#[derive(Debug, Clone)]
pub struct DnsResponseAccessor {
    queues: Arc<DnsResponseQueues>,
}

impl DnsResponseAccessor {
    pub fn push(&self, response: DnsResponse) {
        match response.flow.protocol {
            IpProtocol::Udp => self.queues.udp.lock().push(response),
            IpProtocol::Tcp => unreachable!("Not yet implemented"),
            _ => panic!("Unexpected protocol for DNS Response"),
        }
        // Wake the async executor to process the response
        if let Some(waker) = self.queues.waker.lock().take() {
            waker.wake();
        }
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
    queues: Arc<DnsResponseQueues>,
}

impl DnsResolver {
    #[cfg(windows)]
    pub fn new() -> Result<Self, std::io::Error> {
        use crate::dns_resolver::delay_load::is_dns_raw_apis_supported;
        use crate::dns_resolver::resolver_raw::WindowsDnsResolverBackend;

        if !is_dns_raw_apis_supported() {
            return Err(std::io::ErrorKind::Unsupported.into());
        }

        let queues = Arc::new(DnsResponseQueues {
            udp: Mutex::new(Vec::new()),
            tcp: Mutex::new(Vec::new()),
            waker: Mutex::new(None),
        });
        Ok(Self {
            backend: Box::new(WindowsDnsResolverBackend::new()?),
            queues,
        })
    }

    #[cfg(not(target_os = "windows"))]
    pub fn new() -> Result<Self, std::io::Error> {
        use crate::dns_resolver::resolver::UnixDnsResolverBackend;

        let queues = Arc::new(DnsResponseQueues {
            udp: Mutex::new(Vec::new()),
            tcp: Mutex::new(Vec::new()),
            waker: Mutex::new(None),
        });

        Ok(Self {
            backend: Box::new(UnixDnsResolverBackend::new()?),
            queues,
        })
    }

    pub fn handle_dns(&mut self, request: &DnsRequest<'_>) -> Result<(), DropReason> {
        if request.dns_query.len() <= 12 {
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        let accessor = DnsResponseAccessor {
            queues: self.queues.clone(),
        };

        self.backend.query(request, accessor)
    }

    pub fn poll_responses(
        &mut self,
        cx: &mut Context<'_>,
        protocol: IpProtocol,
    ) -> Vec<DnsResponse> {
        // Register the waker so we get notified when new responses arrive
        *self.queues.waker.lock() = Some(cx.waker().clone());

        match protocol {
            IpProtocol::Udp => self.queues.udp.lock().drain(..).collect(),
            IpProtocol::Tcp => self.queues.tcp.lock().drain(..).collect(),
            _ => unreachable!("Unexpected IpProtocol passed in"),
        }
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
