// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver stub for Unix platforms.
//!
//! This module provides a stub implementation of the DNS resolver for Unix platforms.
//! The actual implementation is not yet available.

use crate::dns_resolver_common::DnsResponse;
use crate::dns_resolver_common::DropReason;
use crate::dns_resolver_common::EthernetAddress;
use crate::dns_resolver_common::IpProtocol;
use crate::dns_resolver_common::Ipv4Address;

/// DNS resolver that manages active DNS queries (Unix stub)
pub struct DnsResolver;

impl DnsResolver {
    /// Create a new DNS resolver instance.
    ///
    /// Returns an error as DNS resolution is not yet implemented for Unix platforms.
    pub fn new() -> Result<Self, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "DNS resolver not implemented for Unix platforms",
        ))
    }

    /// Handle a DNS query by forwarding it to the system DNS resolver.
    ///
    /// # Note
    /// This is a stub implementation for Unix platforms and is not yet implemented.
    pub fn handle_dns(
        &mut self,
        _dns_query: &[u8],
        _protocol: IpProtocol,
        _src_addr: Ipv4Address,
        _dst_addr: Ipv4Address,
        _src_port: u16,
        _dst_port: u16,
        _gateway_mac: EthernetAddress,
        _client_mac: EthernetAddress,
    ) -> Result<(), DropReason> {
        todo!("DNS resolver not yet implemented for Unix platforms")
    }

    /// Poll for completed DNS responses.
    /// Returns the next available response, if any.
    ///
    /// # Note
    /// This is a stub implementation for Unix platforms and is not yet implemented.
    pub fn poll_responses(&mut self, _protocol: IpProtocol) -> Option<DnsResponse> {
        None
    }

    /// Cancel all active DNS queries
    ///
    /// # Note
    /// This is a stub implementation for Unix platforms and is not yet implemented.
    pub fn cancel_all(&mut self) {
        todo!("DNS resolver not yet implemented for Unix platforms")
    }
}
