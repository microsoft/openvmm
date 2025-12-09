// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver using Windows DNS APIs.
//!
//! This module provides a Rust wrapper around the Windows DNS APIs.
//! It prefers the newer DnsQueryRaw APIs when available
//! falling back to DnsQueryEx on older systems.
//!
//! ## Module Organization
//!
//! - [`dns_wire`]: DNS wire format parsing and building (RFC 1035)
//! - [`delay_load`]: Runtime loading of dnsapi.dll functions
//! - [`backend`]: Common trait and types for DNS backends
//! - [`backend_raw`]: DnsQueryRaw implementation
//! - [`backend_ex`]: DnsQueryEx implementation
//!
//! ## API Selection
//!
//! The DnsQueryRaw APIs allow direct raw DNS wire format
//! processing, while DnsQueryEx requires parsing the query
//! and rebuilding the response from Windows DNS record structures.

mod backend;
mod backend_ex;
mod backend_raw;
mod delay_load;
mod dns_wire;

use backend::DnsBackend;
use backend::SharedState;
use backend_ex::ExDnsBackend;
use backend_raw::RawDnsBackend;
use delay_load::get_module;
use delay_load::is_dns_query_ex_supported;
use delay_load::is_dns_raw_apis_supported;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::sync::Arc;

use crate::DnsResponse;
use crate::DropReason;
/// DNS resolver that manages active DNS queries using Windows DNS APIs.
///
/// This resolver automatically selects the best available Windows DNS API:
/// - **DnsQueryRaw** (Windows 11+): Preferred, handles raw DNS wire format
/// - **DnsQueryEx** (Windows 8+): Fallback, requires parsing/rebuilding
///
/// # Example
///
/// ```ignore
/// let resolver = DnsResolver::new()?;
///
/// // Submit a DNS query
/// resolver.handle_dns(
///     dns_query_bytes,
///     IpProtocol::Udp,
///     src_addr, dst_addr,
///     src_port, dst_port,
///     gateway_mac, client_mac,
/// )?;
///
/// // Poll for responses
/// while let Some(response) = resolver.poll_responses(IpProtocol::Udp) {
///     // Send response back to client
/// }
/// ```
pub struct DnsResolver {
    /// The active DNS backend implementation.
    backend: Box<dyn DnsBackend>,
    /// Shared state for responses and cancel handles.
    shared_state: Arc<SharedState>,
}

impl DnsResolver {
    /// Creates a new DNS resolver instance with default configuration.
    ///
    /// Returns an error if no supported DNS APIs are available.
    pub fn new() -> Result<Self, std::io::Error> {
        // Ensure dnsapi.dll is available
        get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        let shared_state = Arc::new(SharedState::new());

        // Determine which backend to use
        let backend: Box<dyn DnsBackend> = if is_dns_raw_apis_supported() {
            tracing::info!("Using DnsQueryRaw APIs");
            Box::new(RawDnsBackend::new(shared_state.clone()))
        } else if is_dns_query_ex_supported() {
            tracing::info!("Using DnsQueryEx APIs");
            Box::new(ExDnsBackend::new(shared_state.clone()))
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "No supported DNS APIs available",
            ));
        };

        Ok(Self {
            backend,
            shared_state,
        })
    }

    /// Submits a DNS query for resolution.
    pub fn handle_dns(
        &mut self,
        dns_query: &[u8],
        protocol: IpProtocol,
        src_addr: Ipv4Address,
        dst_addr: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        gateway_mac: EthernetAddress,
        client_mac: EthernetAddress,
    ) -> Result<(), DropReason> {
        self.backend.query(
            dns_query,
            protocol,
            src_addr,
            dst_addr,
            src_port,
            dst_port,
            gateway_mac,
            client_mac,
        )
    }

    /// Polls for completed DNS responses matching the given protocol.
    ///
    /// Returns `None` if the protocol is not UDP or TCP, or if no responses
    /// are available for the specified protocol.
    pub fn poll_responses(&mut self, protocol: IpProtocol) -> Option<DnsResponse> {
        if protocol != IpProtocol::Udp && protocol != IpProtocol::Tcp {
            return None;
        }

        let mut queue = self.shared_state.response_queue.lock();
        match queue.front() {
            Some(resp) if resp.protocol == protocol => queue.pop_front(),
            _ => None,
        }
    }

    /// Cancels all pending DNS queries.
    pub fn cancel_all(&mut self) {
        self.backend.cancel_all();
        self.shared_state.active_cancel_handles.lock().clear();
    }
}

impl Drop for DnsResolver {
    fn drop(&mut self) {
        self.cancel_all();
    }
}
