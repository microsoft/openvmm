// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver using Windows DNS Raw APIs.
//!
//! This module provides a Rust wrapper around the Windows DNS Raw APIs
//! (DnsQueryRaw, DnsCancelQueryRaw, DnsQueryRawResultFree) that allow
//! for raw DNS query processing similar to the WSL DnsResolver implementation.

// UNSAFETY: This module uses unsafe code to interface with Windows APIs and for FFI bindings.
#![expect(unsafe_code)]
#![allow(unused_doc_comments, missing_docs, unused_imports)]
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use winapi;
use windows_sys::Win32::NetworkManagement::Dns::DNS_PROTOCOL_TCP;
use windows_sys::Win32::NetworkManagement::Dns::DNS_PROTOCOL_UDP;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_NO_MULTICAST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_CANCEL;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_OPTION_BEST_EFFORT_PARSE;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_REQUEST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_REQUEST_0;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_REQUEST_VERSION1;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_RESULT;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_RESULTS_VERSION1;

use crate::DnsResponse;
use crate::DropReason;

/// Delay-load bindings for Windows DNS Raw APIs
pal::delayload! {"dnsapi.dll" {
    pub fn DnsQueryRaw(
        request: *const DNS_QUERY_RAW_REQUEST,
        cancel: *mut DNS_QUERY_RAW_CANCEL
    ) -> i32;

    pub fn DnsCancelQueryRaw(
        cancel: *const DNS_QUERY_RAW_CANCEL
    ) -> i32;

    pub fn DnsQueryRawResultFree(
        result: *mut DNS_QUERY_RAW_RESULT
    ) -> ();
}}

// DNS query context for active requests
struct DnsQueryContext {
    id: u64,
    protocol: IpProtocol,
    cancel_handle: DNS_QUERY_RAW_CANCEL,
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    gateway_mac: EthernetAddress,
    client_mac: EthernetAddress,
    response_queue: Arc<Mutex<VecDeque<DnsResponse>>>,
}

/// DNS resolver that manages active DNS queries using Windows DNS APIs.
pub struct DnsResolver {
    next_request_id: AtomicU64,
    active_requests: Arc<Mutex<HashMap<u64, Box<DnsQueryContext>>>>,
    response_queue: Arc<Mutex<VecDeque<DnsResponse>>>,
}

impl DnsResolver {
    /// Creates a new DNS resolver instance.
    pub fn new() -> Result<Self, std::io::Error> {
        // Ensure the DNS Raw APIs are available on this platform
        get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        Ok(Self {
            next_request_id: AtomicU64::new(0),
            active_requests: Arc::new(Mutex::new(HashMap::new())),
            response_queue: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// Handle a DNS query by forwarding it to the Windows DNS resolver.
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
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        if dns_query.len() < 12 {
            tracing::error!(request_id, "DNS query too short");
            return Err(DropReason::Packet(smoltcp::Error::Dropped));
        }

        // Create a mutable copy of the DNS query
        let mut dns_query_vec = dns_query.to_vec();

        // Create the context
        let mut context = Box::new(DnsQueryContext {
            id: request_id,
            protocol,
            cancel_handle: DNS_QUERY_RAW_CANCEL::default(),
            src_addr,
            dst_addr,
            src_port,
            dst_port,
            gateway_mac,
            client_mac,
            response_queue: self.response_queue.clone(),
        });

        let context_ptr = &mut *context as *mut DnsQueryContext as *mut core::ffi::c_void;

        // Build the DNS request structure
        let dns_protocol = match protocol {
            IpProtocol::Tcp => DNS_PROTOCOL_TCP,
            IpProtocol::Udp => DNS_PROTOCOL_UDP,
            _ => return Err(DropReason::Packet(smoltcp::Error::Dropped)),
        };

        let request = DNS_QUERY_RAW_REQUEST {
            version: DNS_QUERY_RAW_REQUEST_VERSION1,
            resultsVersion: DNS_QUERY_RAW_RESULTS_VERSION1,
            dnsQueryRawSize: dns_query_vec.len() as u32,
            dnsQueryRaw: dns_query_vec.as_mut_ptr(),
            dnsQueryName: std::ptr::null_mut(),
            dnsQueryType: 0,
            queryOptions: DNS_QUERY_NO_MULTICAST as u64
                | DNS_QUERY_RAW_OPTION_BEST_EFFORT_PARSE as u64,
            interfaceIndex: 0,
            queryCompletionCallback: Some(dns_query_raw_callback),
            queryContext: context_ptr,
            queryRawOptions: 0,
            customServersSize: 0,
            customServers: std::ptr::null_mut(),
            protocol: dns_protocol,
            Anonymous: DNS_QUERY_RAW_REQUEST_0::default(),
        };

        // Store the context before making the call
        {
            let mut requests = self.active_requests.lock().unwrap();
            assert!(!requests.contains_key(&request_id), "Request ID collision");
            requests.insert(request_id, context);
        }

        // Make the DNS query
        let result = unsafe {
            let mut requests = self.active_requests.lock().unwrap();
            let context = requests.get_mut(&request_id).unwrap();
            DnsQueryRaw(&request, &mut context.cancel_handle)
        };

        if result != 0 && result != 9506 {
            // 9506 is DNS_REQUEST_PENDING
            tracing::error!(request_id, result, "DnsQueryRaw failed");

            // Remove the context on failure
            let mut requests = self.active_requests.lock().unwrap();
            requests.remove(&request_id);
            drop(requests);
            return Err(DropReason::DnsError(result));
        }

        tracing::debug!(request_id, "DNS query submitted successfully");
        Ok(())
    }

    /// Cancel all active DNS queries
    pub fn cancel_all(&mut self) {
        let mut requests = self.active_requests.lock().unwrap();
        requests.iter().for_each(|(_, ctx)| unsafe {
            DnsCancelQueryRaw(&ctx.cancel_handle);
        });
        requests.clear();
    }

    /// Poll for completed DNS responses.
    /// Returns the next available response, if any.
    pub fn poll_responses(&mut self, protocol: IpProtocol) -> Option<DnsResponse> {
        assert!(
            protocol == IpProtocol::Udp || protocol == IpProtocol::Tcp,
            "protocol must be UDP or TCP"
        );

        let mut queue = self.response_queue.lock().unwrap();
        match queue.front() {
            Some(resp) if resp.protocol == protocol => queue.pop_front(),
            _ => None,
        }
    }
}

/// DNS query completion callback
unsafe extern "system" fn dns_query_raw_callback(
    query_context: *const core::ffi::c_void,
    query_results: *const DNS_QUERY_RAW_RESULT,
) {
    if query_context.is_null() {
        tracing::error!("DNS callback received null context");
        return;
    }

    // Convert context back to a Box
    let context_ptr = query_context as *mut DnsQueryContext;
    let context = unsafe { Box::from_raw(context_ptr) };

    // Process the results
    let dns_response_data = if query_results.is_null() {
        tracing::warn!(request_id = context.id, "DNS query returned null results");
        None
    } else {
        let results = unsafe { &*query_results };

        tracing::debug!(
            request_id = context.id,
            status = results.queryStatus,
            response_size = results.queryRawResponseSize,
            "DNS query completed"
        );

        if results.queryRawResponse.is_null() || results.queryRawResponseSize == 0 {
            None
        } else {
            // Copy the DNS response
            let response_slice = unsafe {
                std::slice::from_raw_parts(
                    results.queryRawResponse,
                    results.queryRawResponseSize as usize,
                )
            };
            Some(response_slice.to_vec())
        }
    };

    // Free the query results
    if !query_results.is_null() {
        unsafe {
            DnsQueryRawResultFree(query_results as *mut _);
        }
    }

    // Queue the response for the main thread to process
    if let Some(response_data) = dns_response_data {
        let response = DnsResponse {
            src_addr: context.src_addr,
            dst_addr: context.dst_addr,
            src_port: context.src_port,
            dst_port: context.dst_port,
            gateway_mac: context.gateway_mac,
            client_mac: context.client_mac,
            response_data,
            protocol: context.protocol,
        };
        context.response_queue.lock().unwrap().push_back(response);
    }
}
