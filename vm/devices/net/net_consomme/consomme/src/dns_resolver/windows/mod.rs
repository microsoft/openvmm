// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows DNS resolver backend implementation using DnsQueryRaw API.
//!
// UNSAFETY: FFI calls to Windows DNS API functions.
#![expect(unsafe_code)]

mod api;

use super::DnsRequestInternal;
use super::build_servfail_response;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsFlow;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
use mesh_channel_core::Sender;
use parking_lot::Mutex;
use slab::Slab;
use std::ptr::null_mut;
use std::sync::Arc;
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
use windows_sys::Win32::Foundation::NO_ERROR;
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

fn push_servfail_response(sender: &Sender<DnsResponse>, flow: &DnsFlow, query: &[u8]) {
    let response = build_servfail_response(query);
    sender.send(DnsResponse {
        flow: flow.clone(),
        response_data: response,
    });
}

fn is_dns_raw_apis_supported() -> bool {
    api::is_supported::DnsQueryRaw()
        && api::is_supported::DnsCancelQueryRaw()
        && api::is_supported::DnsQueryRawResultFree()
}

/// Context passed to the DNS query callback.
struct RawCallbackContext {
    request_id: usize,
    request: DnsRequestInternal,
    pending_requests: Arc<Mutex<Slab<DNS_QUERY_RAW_CANCEL>>>,
}

pub struct WindowsDnsResolverBackend {
    /// Map of pending DNS requests (for cancellation support).
    pending_requests: Arc<Mutex<Slab<DNS_QUERY_RAW_CANCEL>>>,
}

impl WindowsDnsResolverBackend {
    pub fn new() -> Result<Self, std::io::Error> {
        if !is_dns_raw_apis_supported() {
            return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        }

        Ok(WindowsDnsResolverBackend {
            pending_requests: Arc::new(Mutex::new(Slab::new())),
        })
    }
}

impl DnsBackend for WindowsDnsResolverBackend {
    fn query(&self, request: &DnsRequest<'_>, response_sender: Sender<DnsResponse>) {
        // Clone the sender for error handling
        let response_sender_clone = response_sender.clone();

        // For TCP, DnsQueryRaw expects the 2-byte TCP length prefix in the
        // query buffer. Prepend it here so that the DnsTcpHandler can remain
        // platform-agnostic and always pass raw DNS bytes.
        let wire_query = match request.flow.transport {
            super::DnsTransport::Tcp => {
                let len = request.dns_query.len() as u16;
                let mut buf = Vec::with_capacity(2 + request.dns_query.len());
                buf.extend_from_slice(&len.to_be_bytes());
                buf.extend_from_slice(request.dns_query);
                buf
            }
            super::DnsTransport::Udp => request.dns_query.to_vec(),
        };

        // Create internal request with raw DNS bytes (no TCP prefix) so that
        // SERVFAIL generation works correctly.
        let internal_request = DnsRequestInternal {
            flow: request.flow.clone(),
            query: request.dns_query.to_vec(),
            response_sender,
        };

        let dns_query_size = wire_query.len() as u32;
        let dns_query = wire_query.as_ptr().cast_mut();

        // Pre-insert placeholder before calling DnsQueryRaw to avoid race condition
        // where callback fires before we can insert the cancel handle.
        let request_id = self
            .pending_requests
            .lock()
            .insert(DNS_QUERY_RAW_CANCEL::default());

        // Create callback context
        let context = Box::new(RawCallbackContext {
            request_id,
            request: internal_request,
            pending_requests: self.pending_requests.clone(),
        });
        let context_ptr = Box::into_raw(context);

        // Prepare the DNS query request structure
        let mut cancel_handle = DNS_QUERY_RAW_CANCEL::default();

        let dns_request = DNS_QUERY_RAW_REQUEST {
            version: DNS_QUERY_RAW_REQUEST_VERSION1,
            resultsVersion: DNS_QUERY_RAW_RESULTS_VERSION1,
            dnsQueryRawSize: dns_query_size,
            dnsQueryRaw: dns_query,
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
            protocol: match request.flow.transport {
                super::DnsTransport::Tcp => DNS_PROTOCOL_TCP,
                super::DnsTransport::Udp => DNS_PROTOCOL_UDP,
            },
            Anonymous: DNS_QUERY_RAW_REQUEST_0::default(),
        };

        // SAFETY: We're calling the Windows DNS API with properly initialized structures.
        // The query buffer is valid for the duration of the call, and the callback context
        // will remain valid until the callback executes or we cancel the request.
        let result = unsafe { api::DnsQueryRaw(&dns_request, &mut cancel_handle) };

        if result == DNS_REQUEST_PENDING {
            // Update with real cancel handle (only if entry still exists).
            // If the callback already fired and removed the entry, this is a no-op.
            {
                let mut pending = self.pending_requests.lock();
                if let Some(v) = pending.get_mut(request_id) {
                    *v = cancel_handle;
                }
            }
        } else {
            // Remove placeholder since callback won't fire on error
            self.pending_requests.lock().remove(request_id);
            tracelimit::warn_ratelimited!("DnsQueryRaw failed with error code: {}", result);
            // SAFETY: We're reclaiming ownership of the context we just created
            unsafe {
                let _ = Box::from_raw(context_ptr);
            }
            // Return SERVFAIL response
            push_servfail_response(&response_sender_clone, &request.flow, request.dns_query);
        }
    }
}

impl WindowsDnsResolverBackend {
    fn cancel_all(&mut self) {
        let mut pending = self.pending_requests.lock();

        // Cancel all pending requests
        for cancel_handle in pending.drain() {
            // SAFETY: We're calling DnsCancelQueryRaw with a valid cancel handle.
            let result = unsafe { api::DnsCancelQueryRaw(&cancel_handle) };
            if result != NO_ERROR as i32 {
                tracelimit::warn_ratelimited!(
                    "Failed to cancel DNS request: error code {}",
                    result
                );
            }
        }
    }
}

impl Drop for WindowsDnsResolverBackend {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// Error type for DNS query result processing failures
#[derive(Debug)]
enum DnsResultError {
    NullResults,
    QueryFailed(i32),
    NoResponseData,
}

/// Process the DNS query results and extract response data if successful.
///
/// # Safety
///
/// `query_results` must be a valid pointer to a `DNS_QUERY_RAW_RESULT` allocated by Windows.
unsafe fn process_dns_results(
    query_results: *const DNS_QUERY_RAW_RESULT,
) -> Result<Vec<u8>, DnsResultError> {
    // Query results could be null if the query was cancelled
    // SAFETY: if query_results is not null, then it is a valid pointer provided by Windows
    let results = unsafe { query_results.as_ref().ok_or(DnsResultError::NullResults)? };

    if results.queryRawResponseSize > 0 && !results.queryRawResponse.is_null() {
        // SAFETY: queryRawResponse points to a buffer of queryRawResponseSize bytes allocated by Windows
        let response_data = unsafe {
            std::slice::from_raw_parts(
                results.queryRawResponse,
                results.queryRawResponseSize as usize,
            )
        };
        Ok(response_data.to_vec())
    } else if results.queryStatus != NO_ERROR as i32 {
        Err(DnsResultError::QueryFailed(results.queryStatus))
    } else {
        Err(DnsResultError::NoResponseData)
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
    // SAFETY: The context pointer was created by us in query() and is valid.
    let context = unsafe { Box::from_raw(query_context.cast::<RawCallbackContext>().cast_mut()) };

    {
        let mut pending = context.pending_requests.lock();
        pending.remove(context.request_id);
    }

    // SAFETY: query_results is provided by Windows and will be freed after processing
    let response = match unsafe { process_dns_results(query_results) } {
        Ok(mut response_data) => {
            // For TCP, DnsQueryRaw returns the response with a 2-byte TCP
            // length prefix. Strip it so the DnsTcpHandler can add its own
            // framing.
            if context.request.flow.transport == super::DnsTransport::Tcp
                && response_data.len() >= 2
            {
                response_data.drain(..2);
            }
            Some(DnsResponse {
                flow: context.request.flow.clone(),
                response_data,
            })
        }
        Err(DnsResultError::QueryFailed(status)) => {
            tracelimit::warn_ratelimited!(status, "DNS query failed, returning SERVFAIL");
            None
        }
        Err(e) => {
            tracelimit::warn_ratelimited!(error = ?e, "DNS query failed, returning SERVFAIL");
            None
        }
    };

    match response {
        Some(resp) => context.request.response_sender.send(resp),
        None => push_servfail_response(
            &context.request.response_sender,
            &context.request.flow,
            &context.request.query,
        ),
    }

    // Free the results if they were provided
    if !query_results.is_null() {
        // SAFETY: We're calling the Windows API to free memory it allocated
        unsafe {
            api::DnsQueryRawResultFree(query_results.cast_mut());
        }
    }
}
