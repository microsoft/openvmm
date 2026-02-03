// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows DNS resolver backend implementation using DnsQueryRaw API.
//!
// UNSAFETY: FFI calls to Windows DNS API functions.
#![expect(unsafe_code)]

mod api;

use super::DnsRequestInternal;
use super::build_servfail_response;
use crate::DropReason;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsFlow;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
use mesh_channel_core::Sender;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
use windows_sys::Win32::Foundation::NO_ERROR;
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
    request_id: u64,
    request: DnsRequestInternal,
    pending_requests: Arc<Mutex<HashMap<u64, DNS_QUERY_RAW_CANCEL>>>,
}

pub struct WindowsDnsResolverBackend {
    /// Counter for generating unique request IDs.
    next_request_id: AtomicU64,
    /// Map of pending DNS requests (for cancellation support).
    pending_requests: Arc<Mutex<HashMap<u64, DNS_QUERY_RAW_CANCEL>>>,
}

impl WindowsDnsResolverBackend {
    pub fn new() -> Result<Self, std::io::Error> {
        if !is_dns_raw_apis_supported() {
            return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        }

        Ok(WindowsDnsResolverBackend {
            next_request_id: AtomicU64::new(1),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

impl DnsBackend for WindowsDnsResolverBackend {
    fn query(&self, request: &DnsRequest<'_>, response_sender: Sender<DnsResponse>) {
        // Generate unique request ID
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // Clone the sender for error handling
        let response_sender_clone = response_sender.clone();

        // Create internal request
        let internal_request = DnsRequestInternal {
            flow: request.flow.clone(),
            query: request.dns_query.to_vec(),
            response_sender,
        };

        let dns_query_size = internal_request.query.len() as u32;
        let dns_query = internal_request.query.as_ptr().cast_mut();

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
            protocol: DNS_PROTOCOL_UDP,
            Anonymous: DNS_QUERY_RAW_REQUEST_0::default(),
        };

        // Pre-insert placeholder before calling DnsQueryRaw to avoid race condition
        // where callback fires before we can insert the cancel handle.
        self.pending_requests
            .lock()
            .insert(request_id, DNS_QUERY_RAW_CANCEL::default());

        // SAFETY: We're calling the Windows DNS API with properly initialized structures.
        // The query buffer is valid for the duration of the call, and the callback context
        // will remain valid until the callback executes or we cancel the request.
        let result = unsafe { api::DnsQueryRaw(&dns_request, &mut cancel_handle) };

        if result == DNS_REQUEST_PENDING {
            // Update with real cancel handle (only if entry still exists).
            // If the callback already fired and removed the entry, this is a no-op.
            {
                let mut pending = self.pending_requests.lock();
                if let Some(v) = pending.get_mut(&request_id) {
                    *v = cancel_handle;
                }
            }
        } else {
            // Remove placeholder since callback won't fire on error
            self.pending_requests.lock().remove(&request_id);
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
        for (request_id, cancel_handle) in pending.drain() {
            // SAFETY: We're calling DnsCancelQueryRaw with a valid cancel handle.
            let result = unsafe { api::DnsCancelQueryRaw(&cancel_handle) };
            if result != NO_ERROR as i32 {
                tracelimit::warn_ratelimited!(
                    "Failed to cancel DNS request {}: error code {}",
                    request_id,
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
    let request_id = context.request_id;

    // Helper to push SERVFAIL response for this request
    let push_servfail = || {
        push_servfail_response(
            &context.request.response_sender,
            &context.request.flow,
            &context.request.query,
        );
    };

    {
        let mut pending = context.pending_requests.lock();
        pending.remove(&request_id);
    }

    // Process the results if available
    if !query_results.is_null() {
        // SAFETY: query_results is a valid pointer provided by Windows
        let results = unsafe { &*query_results };

        if results.queryStatus == NO_ERROR as i32 {
            // Check if we have valid response data in queryRawResponse
            if results.queryRawResponseSize > 0 && !results.queryRawResponse.is_null() {
                // Extract the response data
                // SAFETY: queryRawResponse points to a buffer of queryRawResponseSize bytes allocated by Windows
                let response_data = unsafe {
                    std::slice::from_raw_parts(
                        results.queryRawResponse,
                        results.queryRawResponseSize as usize,
                    )
                };

                // Push the successful response
                context.request.response_sender.send(DnsResponse {
                    flow: context.request.flow.clone(),
                    response_data: response_data.to_vec(),
                });
            } else {
                // Query succeeded but no data returned
                tracelimit::warn_ratelimited!(
                    "DNS query succeeded but returned no data, returning SERVFAIL"
                );
                push_servfail();
            }
        } else {
            tracelimit::warn_ratelimited!(
                status = results.queryStatus,
                "DNS query failed, returning SERVFAIL"
            );
            push_servfail();
        }

        // SAFETY: We're calling the Windows API to free memory it allocated
        unsafe {
            api::DnsQueryRawResultFree(query_results.cast_mut());
        }
    } else {
        // No results provided, return SERVFAIL
        tracelimit::warn_ratelimited!("DNS callback received null results, returning SERVFAIL");
        push_servfail();
    }
}
