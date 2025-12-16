// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows DNS resolver backend implementation using DnsQueryRaw API.
//!
// UNSAFETY: FFI calls to Windows DNS API functions.
#![expect(unsafe_code)]

use super::DnsRequestInternal;
use super::build_servfail_response;
use crate::DropReason;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
use crate::dns_resolver::DnsResponseAccessor;
use crate::dns_resolver::delay_load::get_dns_cancel_query_raw_fn;
use crate::dns_resolver::delay_load::get_dns_query_raw_fn;
use crate::dns_resolver::delay_load::get_dns_query_raw_result_free_fn;
use crate::dns_resolver::delay_load::get_module;
use crate::dns_resolver::delay_load::is_dns_raw_apis_supported;
use parking_lot::Mutex;
use smoltcp::wire::IpProtocol;
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

/// Wrapper for raw pointer that implements Send
/// SAFETY: The context pointer is managed carefully and only accessed from
/// the callback or during cleanup while holding the mutex.
struct SendPtr(*mut RawCallbackContext);

unsafe impl Send for SendPtr {}

impl SendPtr {
    fn new(ptr: *mut RawCallbackContext) -> Self {
        SendPtr(ptr)
    }

    fn get(&self) -> *mut RawCallbackContext {
        self.0
    }
}

/// Tracked request information for pending DNS queries.
struct TrackedRequest {
    /// The cancel handle returned by DnsQueryRaw.
    cancel_handle: DNS_QUERY_RAW_CANCEL,
    /// The original request data, kept for SERVFAIL generation if needed.
    request: DnsRequestInternal,
    /// The callback context pointer, wrapped for Send safety.
    context_ptr: SendPtr,
}

/// Context passed to the DNS query callback.
struct RawCallbackContext {
    /// Unique identifier for this request.
    request_id: u64,
    /// Reference to the backend's pending requests map.
    pending_requests: Arc<Mutex<HashMap<u64, TrackedRequest>>>,
}

pub struct WindowsDnsResolverBackend {
    /// Counter for generating unique request IDs.
    next_request_id: AtomicU64,
    /// Map of pending DNS requests.
    pending_requests: Arc<Mutex<HashMap<u64, TrackedRequest>>>,
}

impl WindowsDnsResolverBackend {
    pub fn new() -> Result<Self, std::io::Error> {
        get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

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
    fn query(
        &self,
        request: &DnsRequest<'_>,
        accessor: DnsResponseAccessor,
    ) -> Result<(), DropReason> {
        // Only support UDP protocol on Windows
        if request.flow.protocol != IpProtocol::Udp {
            tracing::warn!(
                "TCP DNS queries not supported on Windows backend, only UDP is supported"
            );
            return Err(DropReason::Packet(smoltcp::wire::Error));
        }

        // Generate unique request ID
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // Clone the accessor for error handling
        let accessor_clone = accessor.clone();

        // Create internal request
        let internal_request = DnsRequestInternal {
            flow: request.flow.clone(),
            query: request.dns_query.to_vec(),
            accessor,
        };

        // Create callback context
        let context = Box::new(RawCallbackContext {
            request_id,
            pending_requests: self.pending_requests.clone(),
        });
        let context_ptr = Box::into_raw(context);

        // Prepare the DNS query request structure
        let mut cancel_handle = DNS_QUERY_RAW_CANCEL::default();

        let dns_request = DNS_QUERY_RAW_REQUEST {
            version: DNS_QUERY_RAW_REQUEST_VERSION1,
            resultsVersion: DNS_QUERY_RAW_RESULTS_VERSION1,
            dnsQueryRawSize: internal_request.query.len() as u32,
            dnsQueryRaw: internal_request.query.as_ptr() as *mut u8,
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

        // Call DnsQueryRaw
        // SAFETY: We're calling the Windows DNS API with properly initialized structures.
        // The query buffer is valid for the duration of the call, and the callback context
        // will remain valid until the callback executes or we cancel the request.
        let result = unsafe {
            let dns_query_raw = get_dns_query_raw_fn()
                .map_err(|e| {
                    // Clean up context on error
                    let _ = Box::from_raw(context_ptr);
                    std::io::Error::from_raw_os_error(e as i32)
                })
                .map_err(|_| DropReason::Packet(smoltcp::wire::Error))?;

            dns_query_raw(&dns_request, &mut cancel_handle)
        };

        if result == DNS_REQUEST_PENDING as i32 {
            // Query is pending, store tracking information
            let tracked = TrackedRequest {
                cancel_handle,
                request: internal_request,
                context_ptr: SendPtr::new(context_ptr),
            };
            self.pending_requests.lock().insert(request_id, tracked);
            Ok(())
        } else {
            // Query failed immediately
            tracing::error!("DnsQueryRaw failed with error code: {}", result);

            // Clean up context
            // SAFETY: We're reclaiming ownership of the context we just created
            unsafe {
                let _ = Box::from_raw(context_ptr);
            }

            // Return SERVFAIL response
            let response = build_servfail_response(request.dns_query);
            accessor_clone.push(DnsResponse {
                flow: request.flow.clone(),
                response_data: response,
            });

            Ok(())
        }
    }

    fn cancel_all(&mut self) -> Result<(), std::io::Error> {
        let mut pending = self.pending_requests.lock();

        // Get the cancel function
        let cancel_fn = unsafe {
            get_dns_cancel_query_raw_fn()
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?
        };

        // Cancel all pending requests
        for (request_id, tracked) in pending.drain() {
            // SAFETY: We're calling DnsCancelQueryRaw with a valid cancel handle
            unsafe {
                let result = cancel_fn(&tracked.cancel_handle);
                if result != NO_ERROR as i32 {
                    tracing::warn!(
                        "Failed to cancel DNS request {}: error code {}",
                        request_id,
                        result
                    );
                }

                // Clean up the context
                // The callback may or may not have been called at this point,
                // but we own the context pointer and must free it
                let _ = Box::from_raw(tracked.context_ptr.get());
            }
        }

        Ok(())
    }
}

impl Drop for WindowsDnsResolverBackend {
    fn drop(&mut self) {
        let _ = self.cancel_all();
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
    // Validate inputs
    if query_context.is_null() {
        tracing::error!("DNS callback received null context");
        return;
    }

    // SAFETY: The context pointer was created by us in query() and is valid
    let context = unsafe { &*query_context.cast::<RawCallbackContext>() };
    let request_id = context.request_id;

    // Remove the tracked request from the map
    let tracked = {
        let mut pending = context.pending_requests.lock();
        pending.remove(&request_id)
    };

    let Some(tracked) = tracked else {
        tracing::warn!("DNS callback for unknown request ID: {}", request_id);
        return;
    };

    let context_ptr = tracked.context_ptr.get();

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
                tracked.request.accessor.push(DnsResponse {
                    flow: tracked.request.flow.clone(),
                    response_data: response_data.to_vec(),
                });
            } else {
                // Query succeeded but no data returned
                tracing::error!("DNS query succeeded but returned no data, returning SERVFAIL");
                let response = build_servfail_response(&tracked.request.query);
                tracked.request.accessor.push(DnsResponse {
                    flow: tracked.request.flow,
                    response_data: response,
                });
            }
        } else {
            // Query failed, return SERVFAIL
            tracing::error!(
                "DNS query failed with status {}, returning SERVFAIL",
                results.queryStatus
            );
            let response = build_servfail_response(&tracked.request.query);
            tracked.request.accessor.push(DnsResponse {
                flow: tracked.request.flow,
                response_data: response,
            });
        }

        // Free the Windows-allocated result structure
        // SAFETY: We're calling the Windows API to free memory it allocated
        if let Ok(free_fn) = unsafe { get_dns_query_raw_result_free_fn() } {
            unsafe {
                free_fn(query_results as *mut DNS_QUERY_RAW_RESULT);
            }
        } else {
            tracing::error!("Failed to get DnsQueryRawResultFree function");
        }
    } else {
        // No results provided, return SERVFAIL
        tracing::error!("DNS callback received null results, returning SERVFAIL");
        let response = build_servfail_response(&tracked.request.query);
        tracked.request.accessor.push(DnsResponse {
            flow: tracked.request.flow,
            response_data: response,
        });
    }

    // Clean up the callback context
    // SAFETY: We're reclaiming ownership of the context box we created in query()
    unsafe {
        let _ = Box::from_raw(context_ptr);
    }
}
