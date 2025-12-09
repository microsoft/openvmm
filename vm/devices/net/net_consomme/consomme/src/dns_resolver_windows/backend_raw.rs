// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DnsQueryRaw backend implementation (Windows 11+).
//!
//! This backend uses the newer DnsQueryRaw APIs which allow direct processing
//! of raw DNS wire format, avoiding the need to parse and rebuild messages.

// UNSAFETY: FFI calls to Windows DNS APIs and callback handling.
#![expect(unsafe_code)]

use super::backend::CancelHandle;
use super::backend::CancelHandleInner;
use super::backend::DnsBackend;
use super::backend::QueryContext;
use super::backend::RequestIdGenerator;
use super::backend::SharedState;
use super::delay_load::get_dns_cancel_query_raw_fn;
use super::delay_load::get_dns_query_raw_fn;
use super::delay_load::get_dns_query_raw_result_free_fn;
use crate::DropReason;
use crate::PacketError;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::ptr::null_mut;
use std::sync::Arc;
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
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

/// Context passed to the DnsQueryRaw callback.
struct RawCallbackContext {
    /// Query context with routing information.
    query_ctx: QueryContext,
    /// Shared state for queuing responses.
    shared_state: Arc<SharedState>,
}

/// DNS backend using DnsQueryRaw APIs (Windows 11+).
///
/// This backend passes raw DNS wire format directly to/from Windows,
/// avoiding the overhead of parsing and rebuilding DNS messages.
pub struct RawDnsBackend {
    /// Shared state for responses and cancel handles.
    shared_state: Arc<SharedState>,
    /// Request ID generator.
    id_generator: RequestIdGenerator,
}

impl RawDnsBackend {
    /// Create a new Raw DNS backend.
    pub fn new(shared_state: Arc<SharedState>) -> Self {
        Self {
            shared_state,
            id_generator: RequestIdGenerator::new(),
        }
    }
}

impl DnsBackend for RawDnsBackend {
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
    ) -> Result<(), DropReason> {
        let request_id = self.id_generator.next();

        // Validate DNS header (minimum 12 bytes)
        if dns_query.len() < 12 {
            tracing::error!(request_id, len = dns_query.len(), "DNS query too short");
            return Err(DropReason::Packet(PacketError::Dropped));
        }

        // Create a mutable copy of the DNS query for the Windows API
        let mut dns_query_vec = dns_query.to_vec();

        // Create the callback context
        let context = Box::new(RawCallbackContext {
            query_ctx: QueryContext {
                id: request_id,
                protocol,
                src_addr,
                dst_addr,
                src_port,
                dst_port,
                gateway_mac,
                client_mac,
            },
            shared_state: self.shared_state.clone(),
        });

        // Leak the box to pass ownership to the callback via raw pointer
        let context_ptr = Box::into_raw(context);

        // Determine protocol for Windows API
        let dns_protocol = match protocol {
            IpProtocol::Tcp => DNS_PROTOCOL_TCP,
            IpProtocol::Udp => DNS_PROTOCOL_UDP,
            _ => {
                // SAFETY: Reclaim the context on error
                unsafe {
                    let _ = Box::from_raw(context_ptr);
                }
                return Err(DropReason::Packet(PacketError::Dropped));
            }
        };

        let mut cancel_handle = DNS_QUERY_RAW_CANCEL::default();

        let request = DNS_QUERY_RAW_REQUEST {
            version: DNS_QUERY_RAW_REQUEST_VERSION1,
            resultsVersion: DNS_QUERY_RAW_RESULTS_VERSION1,
            dnsQueryRawSize: dns_query_vec.len() as u32,
            dnsQueryRaw: dns_query_vec.as_mut_ptr(),
            dnsQueryName: null_mut(),
            dnsQueryType: 0,
            queryOptions: DNS_QUERY_NO_MULTICAST as u64
                | DNS_QUERY_RAW_OPTION_BEST_EFFORT_PARSE as u64,
            interfaceIndex: 0,
            queryCompletionCallback: Some(dns_query_raw_callback),
            queryContext: context_ptr.cast::<core::ffi::c_void>(),
            queryRawOptions: 0,
            customServersSize: 0,
            customServers: null_mut(),
            protocol: dns_protocol,
            Anonymous: DNS_QUERY_RAW_REQUEST_0::default(),
        };

        // SAFETY: Get and call the DnsQueryRaw function
        let result = match unsafe { get_dns_query_raw_fn() } {
            Ok(fnptr) => {
                // SAFETY: Call DNS query API with valid request
                unsafe { fnptr(&request, &mut cancel_handle) }
            }
            Err(_) => {
                // SAFETY: Free the context on error
                unsafe {
                    let _ = Box::from_raw(context_ptr);
                }
                return Err(DropReason::DnsError);
            }
        };

        if result != 0 && result != DNS_REQUEST_PENDING {
            tracing::error!(request_id, result, "DnsQueryRaw failed");

            // SAFETY: Free the context on error
            unsafe {
                let _ = Box::from_raw(context_ptr);
            }
            return Err(DropReason::DnsError);
        }

        // Store the cancel handle for potential cancellation
        {
            let mut handles = self.shared_state.active_cancel_handles.lock();
            handles.insert(
                request_id,
                CancelHandle {
                    handle: CancelHandleInner::Raw(cancel_handle),
                },
            );
        }

        Ok(())
    }

    fn cancel_all(&mut self) {
        let handles = self.shared_state.active_cancel_handles.lock();

        for (_, cancel_handle) in handles.iter() {
            if let CancelHandleInner::Raw(raw_handle) = &cancel_handle.handle {
                // SAFETY: Get and call the DnsCancelQueryRaw function
                if let Ok(fnptr) = unsafe { get_dns_cancel_query_raw_fn() } {
                    // SAFETY: Call DNS cancel API with valid handle
                    let _ = unsafe { fnptr(raw_handle) };
                }
            }
        }
    }
}

/// Callback for DnsQueryRaw completion.
///
/// # Safety
///
/// The Windows DNS API calls this function when a DNS query completes.
/// The `query_context` must be a valid pointer to a `RawCallbackContext`.
unsafe extern "system" fn dns_query_raw_callback(
    query_context: *const core::ffi::c_void,
    query_results: *const DNS_QUERY_RAW_RESULT,
) {
    if query_context.is_null() {
        tracing::error!("DNS callback received null context");
        return;
    }

    // Convert context back to a Box and take ownership
    let context_ptr = query_context as *mut RawCallbackContext;
    // SAFETY: Take ownership of the context
    let context = unsafe { Box::from_raw(context_ptr) };

    // Remove the cancel handle since the query has completed
    context
        .shared_state
        .active_cancel_handles
        .lock()
        .remove(&context.query_ctx.id);

    // Process the results
    let dns_response_data = if query_results.is_null() {
        tracing::warn!(
            request_id = context.query_ctx.id,
            "DNS query returned null results"
        );
        None
    } else {
        // SAFETY: Dereferencing raw pointer from Windows API
        let results = unsafe { &*query_results };

        tracing::debug!(
            request_id = context.query_ctx.id,
            status = results.queryStatus,
            response_size = results.queryRawResponseSize,
            "DNS query completed"
        );

        if results.queryRawResponse.is_null() || results.queryRawResponseSize == 0 {
            None
        } else {
            // SAFETY: Create a slice from the raw response data
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
        // SAFETY: Get and call the DnsQueryRawResultFree function
        if let Ok(fnptr) = unsafe { get_dns_query_raw_result_free_fn() } {
            // SAFETY: Free DNS query results
            unsafe { fnptr(query_results.cast_mut()) };
        }
    }

    // Queue the response for the main thread to process
    if let Some(response_data) = dns_response_data {
        let response = context.query_ctx.to_response(response_data);
        context
            .shared_state
            .response_queue
            .lock()
            .push_back(response);
    }
}
