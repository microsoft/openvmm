// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver stub for Unix platforms.
//!
//! This module provides a stub implementation of the DNS resolver for Unix platforms.
//! The actual implementation is not yet available and uses `todo!()` macros.

use smoltcp::wire::IpProtocol;

/// DNS resolver that manages active DNS queries (Unix stub)
pub struct DnsResolver;

/// DNS error types
#[derive(Debug)]
pub enum DnsError {
    /// Query failed with the given error code
    QueryFailed(i32),
    /// A query with this ID already exists
    AlreadyExists,
}

impl DnsResolver {
    /// Create a new DNS resolver instance
    pub fn new() -> Result<Self, DnsError> {
        Ok(Self)
    }

    /// Handle a DNS query by forwarding it to the system DNS resolver.
    ///
    /// # Note
    /// This is a stub implementation for Unix platforms and is not yet implemented.
    pub fn handle_dns(
        &mut self,
        _dns_query: &[u8],
        _protocol: IpProtocol,
        _callback: impl FnOnce(Option<Vec<u8>>) + Send + 'static,
    ) -> Result<(), DnsError> {
        todo!("DNS resolver not yet implemented for Unix platforms")
    }

    /// Cancel all active DNS queries
    ///
    /// # Note
    /// This is a stub implementation for Unix platforms and is not yet implemented.
    pub fn cancel_all(&mut self) {
        todo!("DNS resolver not yet implemented for Unix platforms")
    }
}

/// Create a DNS SERVFAIL response for a given query
pub fn create_servfail_response(query: &[u8]) -> Vec<u8> {
    if query.len() < 12 {
        // Invalid DNS query, return minimal SERVFAIL
        return vec![
            0, 0, // Transaction ID
            0x81, 0x82, // Flags: Response, SERVFAIL
            0, 0, // Questions: 0
            0, 0, // Answers: 0
            0, 0, // Authority: 0
            0, 0, // Additional: 0
        ];
    }

    // Copy transaction ID from query
    let transaction_id = [query[0], query[1]];

    // Build SERVFAIL response with same transaction ID
    let mut response = Vec::with_capacity(12 + (query.len() - 12).min(512));
    response.extend_from_slice(&transaction_id);
    response.extend_from_slice(&[
        0x81, 0x82, // Flags: Response, SERVFAIL (RCODE=2)
        0, 0, // Questions: 0
        0, 0, // Answers: 0
        0, 0, // Authority: 0
        0, 0, // Additional: 0
    ]);

    response
}
