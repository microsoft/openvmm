// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DnsQueryEx backend implementation (Windows 8+).
//!
//! This backend uses the DnsQueryEx APIs which require parsing the DNS query
//! and rebuilding the DNS response from Windows DNS records.

// UNSAFETY: FFI calls to Windows DNS APIs and callback handling.
#![expect(unsafe_code)]

use super::backend::CancelHandle;
use super::backend::CancelHandleInner;
use super::backend::DnsBackend;
use super::backend::QueryContext;
use super::backend::RequestIdGenerator;
use super::backend::SharedState;
use super::delay_load::get_dns_cancel_query_fn;
use super::delay_load::get_dns_query_ex_fn;
use super::dns_wire::DnsRecordType;
use super::dns_wire::ParsedDnsQuery;
use super::dns_wire::build_dns_error_response;
use super::dns_wire::build_dns_response;
use super::dns_wire::dns_error_to_rcode;
use super::dns_wire::parse_dns_query;
use crate::DropReason;
use crate::PacketError;
use smoltcp::wire::DnsRcode;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::ptr::null_mut;
use std::sync::Arc;
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_CANCEL;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_NO_MULTICAST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_REQUEST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_REQUEST_VERSION1;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RESULT;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RESULTS_VERSION1;
use windows_sys::Win32::NetworkManagement::Dns::DnsFree;
use windows_sys::Win32::NetworkManagement::Dns::DnsFreeRecordList;

/// Process DNS query results and build a response.
///
/// This is a shared helper used by both the synchronous completion path
/// and the async callback to avoid code duplication.
///
/// # Safety
///
/// The `query_records` pointer must be either null or a valid pointer to
/// a DNS_RECORDA linked list from Windows DNS APIs.
unsafe fn process_dns_query_results(
    parsed_query: &ParsedDnsQuery,
    query_status: i32,
    query_records: *mut windows_sys::Win32::NetworkManagement::Dns::DNS_RECORDA,
) -> Vec<u8> {
    let response_data = if query_status == 0 {
        build_dns_response(parsed_query, query_records, DnsRcode::NoError)
    } else {
        // Convert error to appropriate RCODE
        let rcode = dns_error_to_rcode(query_status as u32);
        build_dns_error_response(parsed_query, rcode)
    };

    // Free the DNS records if any
    if !query_records.is_null() {
        // SAFETY: Free records allocated by Windows
        unsafe { DnsFree(query_records.cast(), DnsFreeRecordList) };
    }

    response_data
}

/// Context passed to the DnsQueryEx callback.
struct ExCallbackContext {
    /// Query context with routing information.
    query_ctx: QueryContext,
    /// Shared state for queuing responses.
    shared_state: Arc<SharedState>,
    /// Parsed query info needed to build the response.
    parsed_query: ParsedDnsQuery,
    /// Query results pointer (owned by this context for async queries).
    query_results: *mut DNS_QUERY_RESULT,
}

// SAFETY: The context is only accessed from the callback thread after creation
unsafe impl Send for ExCallbackContext {}

/// DNS backend using DnsQueryEx APIs.
///
/// This backend parses DNS queries to extract the query name and type,
/// then rebuilds the response from Windows DNS_RECORDA structures.
pub struct ExDnsBackend {
    /// Shared state for responses and cancel handles.
    shared_state: Arc<SharedState>,
    /// Request ID generator.
    id_generator: RequestIdGenerator,
}

impl ExDnsBackend {
    /// Create a new Ex DNS backend.
    pub fn new(shared_state: Arc<SharedState>) -> Self {
        Self {
            shared_state,
            id_generator: RequestIdGenerator::new(),
        }
    }
}

impl DnsBackend for ExDnsBackend {
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

        let parsed_query = match parse_dns_query(dns_query) {
            Some(q) => q,
            None => {
                return Err(DropReason::Packet(PacketError::Dropped));
            }
        };

        // Get the raw query type value for both tracing and Windows API
        let query_type: DnsRecordType = parsed_query.qtype.into();
        if matches!(query_type, DnsRecordType::Unsupported(_)) {
            tracing::warn!(
                request_id = request_id,
                query_type = ?query_type,
                "DNS query type not supported"
            );
            return Err(DropReason::Packet(PacketError::Dropped));
        }

        // Convert to UTF-16 (wide string) for Windows API - DnsQueryEx expects PCWSTR
        let query_name_wide: Vec<u16> = parsed_query
            .name
            .encode_utf16()
            .chain(std::iter::once(0u16)) // null terminator
            .collect();

        let query_results = Box::into_raw(Box::new(DNS_QUERY_RESULT {
            Version: DNS_QUERY_RESULTS_VERSION1,
            QueryStatus: 0,
            QueryOptions: 0,
            pQueryRecords: null_mut(),
            Reserved: null_mut(),
        }));

        // Create the callback context
        let context = Box::new(ExCallbackContext {
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
            parsed_query,
            query_results,
        });

        let context_ptr = Box::into_raw(context);

        let mut cancel_handle = DNS_QUERY_CANCEL::default();

        let request = DNS_QUERY_REQUEST {
            Version: DNS_QUERY_REQUEST_VERSION1,
            QueryName: query_name_wide.as_ptr(),
            QueryType: query_type.as_u16(),
            QueryOptions: DNS_QUERY_NO_MULTICAST as u64,
            pDnsServerList: null_mut(),
            InterfaceIndex: 0,
            pQueryCompletionCallback: Some(dns_query_ex_callback),
            pQueryContext: context_ptr.cast::<core::ffi::c_void>(),
        };

        // SAFETY: Get and call the DnsQueryEx function
        let result = match unsafe { get_dns_query_ex_fn() } {
            Ok(fnptr) => {
                // SAFETY: Call DNS query API with valid request
                unsafe { fnptr(&request, query_results, &mut cancel_handle) }
            }
            Err(_) => {
                // SAFETY: Free the context and results on error
                unsafe {
                    let ctx = Box::from_raw(context_ptr);
                    let _ = Box::from_raw(ctx.query_results);
                }
                return Err(DropReason::DnsError);
            }
        };

        // Handle synchronous completion
        if result != DNS_REQUEST_PENDING {
            // Query completed synchronously
            // SAFETY: Take ownership of context
            let context = unsafe { Box::from_raw(context_ptr) };
            // SAFETY: Take ownership of query_results
            let query_results_box = unsafe { Box::from_raw(context.query_results) };

            // Use the shared helper to process results
            // SAFETY: query_results_box.pQueryRecords is valid or null
            let response_data = unsafe {
                process_dns_query_results(
                    &context.parsed_query,
                    if result == ERROR_SUCCESS as i32 { 0 } else { result },
                    query_results_box.pQueryRecords,
                )
            };

            // Queue the response
            let response = context.query_ctx.to_response(response_data);
            self.shared_state.response_queue.lock().push_back(response);

            return Ok(());
        }

        // Async query pending - store cancel handle
        {
            let mut handles = self.shared_state.active_cancel_handles.lock();
            handles.insert(
                request_id,
                CancelHandle {
                    handle: CancelHandleInner::Ex(cancel_handle)
                },
            );
        }

        Ok(())
    }

    fn cancel_all(&mut self) {
        let handles = self.shared_state.active_cancel_handles.lock();

        for (_, cancel_handle) in handles.iter() {
            if let CancelHandleInner::Ex(ex_handle) = &cancel_handle.handle {
                // SAFETY: Get and call the DnsCancelQuery function
                if let Ok(fnptr) = unsafe { get_dns_cancel_query_fn() } {
                    // SAFETY: Call DNS cancel API with valid handle
                    let _ = unsafe { fnptr(ex_handle as *const _ as *mut _) };
                }
            }
        }
    }
}

/// Callback for DnsQueryEx completion.
///
/// # Safety
///
/// The Windows DNS API calls this function when a DNS query completes.
/// The `query_context` must be a valid pointer to an `ExCallbackContext`.
unsafe extern "system" fn dns_query_ex_callback(
    query_context: *const core::ffi::c_void,
    query_results: *mut DNS_QUERY_RESULT,
) {
    if query_context.is_null() {
        tracing::error!("DNS callback received null context");
        return;
    }

    // Convert context back to a Box and take ownership
    let context_ptr = query_context as *mut ExCallbackContext;
    // SAFETY: Take ownership of the context
    let context = unsafe { Box::from_raw(context_ptr) };

    // Remove the cancel handle since the query has completed
    context
        .shared_state
        .active_cancel_handles
        .lock()
        .remove(&context.query_ctx.id);

    // Process the results - use the query_results parameter from callback
    let dns_response_data = if query_results.is_null() {
        tracing::debug!(
            request_id = context.query_ctx.id,
            "DNS query returned null results"
        );
        build_dns_error_response(&context.parsed_query, DnsRcode::ServFail)
    } else {
        // SAFETY: Dereferencing raw pointer from Windows API
        let results = unsafe { &*query_results };

        tracing::debug!(
            request_id = context.query_ctx.id,
            status = results.QueryStatus,
            query_name = %context.parsed_query.name,
            "DNS query completed via DnsQueryEx"
        );

        // Log additional details for error statuses
        if results.QueryStatus != 0 {
            tracing::warn!(
                request_id = context.query_ctx.id,
                status = results.QueryStatus,
                status_hex = format_args!("0x{:X}", results.QueryStatus),
                query_ctx = ?context.query_ctx,
                parsed_query = ?context.parsed_query,
                "DNS query failed"
            );
        }

        // Use the shared helper to process results
        // SAFETY: results.pQueryRecords is valid or null from Windows API
        unsafe { process_dns_query_results(&context.parsed_query, results.QueryStatus, results.pQueryRecords) }
    };

    // Free the query_results struct we allocated
    // SAFETY: Free the query_results box we created
    let _ = unsafe { Box::from_raw(context.query_results) };

    // Queue the response for the main thread to process
    let response = context.query_ctx.to_response(dns_response_data);
    context
        .shared_state
        .response_queue
        .lock()
        .push_back(response);
}
