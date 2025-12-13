// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS backend trait and shared types.
//!
//! This module defines the common interface for DNS backend implementations
//! and shared data structures used across backends.

use crate::DnsResponse;
use crate::DropReason;
use parking_lot::Mutex;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_CANCEL;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_CANCEL;

/// Unified cancel handle that supports both Raw and Ex APIs.
pub(super) struct CancelHandle {
    /// The actual cancel handle.
    pub handle: CancelHandleInner,
}

/// The inner cancel handle type.
pub(super) enum CancelHandleInner {
    /// Cancel handle for DnsQueryRaw API.
    Raw(DNS_QUERY_RAW_CANCEL),
    /// Cancel handle for DnsQueryEx API.
    Ex(DNS_QUERY_CANCEL),
}

/// Shared state between the DnsResolver and backend callbacks.
///
/// This is wrapped in Arc for thread-safe sharing with async callbacks.
pub(super) struct SharedState {
    /// Queue of completed DNS responses ready to be sent.
    pub response_queue: Mutex<VecDeque<DnsResponse>>,
    /// Active cancel handles for pending queries.
    pub active_cancel_handles: Mutex<HashMap<u64, CancelHandle>>,
}

impl SharedState {
    /// Create a new shared state instance with the specified configuration.
    pub fn new() -> Self {
        Self {
            response_queue: Mutex::new(VecDeque::new()),
            active_cancel_handles: Mutex::new(HashMap::new()),
        }
    }
}

/// Common context for all DNS queries.
///
/// Contains the information needed to route a DNS response back to the client.
#[derive(Clone, Debug)]
pub(super) struct QueryContext {
    /// Unique request ID for tracking.
    pub id: u64,
    /// Transport protocol (UDP or TCP).
    pub protocol: IpProtocol,
    /// Source IP address (the client).
    pub src_addr: Ipv4Address,
    /// Destination IP address (the gateway/DNS server).
    pub dst_addr: Ipv4Address,
    /// Source port (the client's port).
    pub src_port: u16,
    /// Destination port (DNS port, usually 53).
    pub dst_port: u16,
    /// Gateway MAC address.
    pub gateway_mac: EthernetAddress,
    /// Client MAC address.
    pub client_mac: EthernetAddress,
}

impl QueryContext {
    /// Create a DnsResponse from this context and response data.
    pub fn to_response(&self, response_data: Vec<u8>) -> DnsResponse {
        DnsResponse {
            src_addr: self.src_addr,
            dst_addr: self.dst_addr,
            src_port: self.src_port,
            dst_port: self.dst_port,
            gateway_mac: self.gateway_mac,
            client_mac: self.client_mac,
            response_data,
            protocol: self.protocol,
        }
    }
}

/// Trait for DNS backend implementations.
///
/// Each backend handles DNS queries using a specific Windows API
/// (DnsQueryRaw or DnsQueryEx).
pub(super) trait DnsBackend: Send {
    /// Submit a DNS query for async resolution.
    ///
    /// The response will be queued to the shared state's response_queue
    /// when the query completes.
    fn query(
        &mut self,
        dns_query: &[u8],
        protocol: IpProtocol,
        src_addr: Ipv4Address,
        dst_addr: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        gateway_mac: EthernetAddress,
        client_mac: EthernetAddress,
    ) -> Result<(), DropReason>;

    /// Cancel all pending DNS queries.
    fn cancel_all(&mut self);
}

/// Thread-safe request ID generator.
pub(super) struct RequestIdGenerator {
    next_id: AtomicU64,
}

impl RequestIdGenerator {
    /// Create a new ID generator.
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }

    /// Generate the next unique request ID.
    pub fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}
