// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver using Windows DNS Raw APIs.
//!
//! This module provides a Rust wrapper around the Windows DNS Raw APIs
//! (DnsQueryRaw, DnsCancelQueryRaw, DnsQueryRawResultFree) that allow
//! for raw DNS query processing similar to the WSL DnsResolver implementation.

// UNSAFETY: This module uses unsafe code to interface with Windows APIs and for FFI bindings.
#![expect(unsafe_code)]
use parking_lot::Mutex;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Address;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::DNS_REQUEST_PENDING;
use windows_sys::Win32::Foundation::ERROR_PROC_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::WIN32_ERROR;
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
use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;

use crate::DnsResponse;
use crate::DropReason;

// Delay-load helpers for dnsapi.dll functions
fn get_module() -> Result<isize, WIN32_ERROR> {
    static MODULE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());
    let mut module = MODULE.load(Ordering::Relaxed);
    if module.is_null() {
        // SAFETY: FFI call to load dnsapi.dll
        module = unsafe { LoadLibraryA(c"dnsapi.dll".as_ptr().cast()).cast::<core::ffi::c_void>() };
        if module.is_null() {
            // SAFETY: FFI call to get last error code
            return Err(unsafe { GetLastError() });
        }
        MODULE.store(module, Ordering::Relaxed);
    }
    Ok(module as isize)
}

fn get_proc_address(name: &[u8], cache: &AtomicUsize) -> Result<usize, WIN32_ERROR> {
    let mut fnval = cache.load(Ordering::Relaxed);
    if fnval == 0 {
        let module = get_module()?;
        // SAFETY: FFI call to get function address from module
        fnval = unsafe { GetProcAddress(module as _, name.as_ptr()) }
            .map(|f| f as usize)
            .unwrap_or(0);
        cache.store(if fnval == 0 { 1 } else { fnval }, Ordering::Relaxed);
    }
    if fnval == 1 {
        Err(ERROR_PROC_NOT_FOUND)
    } else {
        Ok(fnval)
    }
}

macro_rules! define_dns_api {
    ($fn_name:ident, $api_name:literal, $fn_type:ty) => {
        fn $fn_name() -> Result<usize, WIN32_ERROR> {
            static CACHE: AtomicUsize = AtomicUsize::new(0);
            get_proc_address(concat!($api_name, "\0").as_bytes(), &CACHE)
        }
    };
}

define_dns_api!(
    get_dns_query_raw,
    "DnsQueryRaw",
    unsafe extern "system" fn(*const DNS_QUERY_RAW_REQUEST, *mut DNS_QUERY_RAW_CANCEL) -> i32
);
define_dns_api!(
    get_dns_cancel_query_raw,
    "DnsCancelQueryRaw",
    unsafe extern "system" fn(*const DNS_QUERY_RAW_CANCEL) -> i32
);
define_dns_api!(
    get_dns_query_raw_result_free,
    "DnsQueryRawResultFree",
    unsafe extern "system" fn(*mut DNS_QUERY_RAW_RESULT)
);

fn is_dns_apis_supported() -> bool {
    get_dns_query_raw().is_ok()
        && get_dns_cancel_query_raw().is_ok()
        && get_dns_query_raw_result_free().is_ok()
}

// DNS query context for active requests
struct DnsQueryContext {
    id: u64,
    protocol: IpProtocol,
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    gateway_mac: EthernetAddress,
    client_mac: EthernetAddress,
    response_queue: Arc<Mutex<VecDeque<DnsResponse>>>,
    active_cancel_handles: Arc<Mutex<HashMap<u64, DNS_QUERY_RAW_CANCEL>>>,
}

/// DNS resolver that manages active DNS queries using Windows DNS APIs.
pub struct DnsResolver {
    next_request_id: AtomicU64,
    active_cancel_handles: Arc<Mutex<HashMap<u64, DNS_QUERY_RAW_CANCEL>>>,
    response_queue: Arc<Mutex<VecDeque<DnsResponse>>>,
}

impl DnsResolver {
    /// Creates a new DNS resolver instance.
    pub fn new() -> Result<Self, std::io::Error> {
        // Ensure the DNS APIs are available on this platform
        get_module().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        if !is_dns_apis_supported() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "DNS APIs not available",
            ));
        }

        Ok(Self {
            next_request_id: AtomicU64::new(0),
            active_cancel_handles: Arc::new(Mutex::new(HashMap::new())),
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

        // Create a mutable copy of the DNS query for the Windows API
        let mut dns_query_vec = dns_query.to_vec();

        // Create the context
        let context = Box::new(DnsQueryContext {
            id: request_id,
            protocol,
            src_addr,
            dst_addr,
            src_port,
            dst_port,
            gateway_mac,
            client_mac,
            response_queue: self.response_queue.clone(),
            active_cancel_handles: self.active_cancel_handles.clone(),
        });

        // Leak the box to pass ownership to the callback via raw pointer
        let context_ptr = Box::into_raw(context);

        // Build the DNS request structure
        let dns_protocol = match protocol {
            IpProtocol::Tcp => DNS_PROTOCOL_TCP,
            IpProtocol::Udp => DNS_PROTOCOL_UDP,
            _ => return Err(DropReason::Packet(smoltcp::Error::Dropped)),
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

        let result = match get_dns_query_raw() {
            Ok(fnval) => {
                // SAFETY: Transmute function pointer from usize
                let fnptr: unsafe extern "system" fn(
                    *const DNS_QUERY_RAW_REQUEST,
                    *mut DNS_QUERY_RAW_CANCEL,
                ) -> i32 = unsafe { std::mem::transmute(fnval) };
                // SAFETY: Call DNS query API with valid request
                unsafe { fnptr(&request, &mut cancel_handle) }
            }
            Err(_) => {
                // UNSAFETY: Free the context on error
                unsafe {
                    let _ = Box::from_raw(context_ptr);
                }
                return Err(DropReason::Packet(smoltcp::Error::Dropped));
            }
        };

        if result != 0 && result != DNS_REQUEST_PENDING {
            tracing::error!(request_id, result, "DnsQueryRaw failed");

            // UNSAFETY: Free the context on error
            unsafe {
                let _ = Box::from_raw(context_ptr);
            }
            return Err(DropReason::DnsError(result));
        }

        // Store the cancel handle for potential cancellation
        {
            let mut handles = self.active_cancel_handles.lock();
            handles.insert(request_id, cancel_handle);
        }

        Ok(())
    }

    /// Cancel all active DNS queries
    pub fn cancel_all(&mut self) {
        let mut handles = self.active_cancel_handles.lock();
        if let Ok(fnval) = get_dns_cancel_query_raw() {
            // SAFETY: Transmute function pointer from usize
            let fnptr: unsafe extern "system" fn(*const DNS_QUERY_RAW_CANCEL) -> i32 =
                unsafe { std::mem::transmute(fnval) };
            handles.iter().for_each(|(_, cancel_handle)| {
                // SAFETY: Call DNS cancel API with valid handle
                let _ = unsafe { fnptr(cancel_handle) };
            });
        }
        handles.clear();
    }

    /// Poll for completed DNS responses.
    /// Returns the next available response, if any.
    pub fn poll_responses(&mut self, protocol: IpProtocol) -> Option<DnsResponse> {
        assert!(
            protocol == IpProtocol::Udp || protocol == IpProtocol::Tcp,
            "protocol must be UDP or TCP"
        );

        let mut queue = self.response_queue.lock();
        match queue.front() {
            Some(resp) if resp.protocol == protocol => queue.pop_front(),
            _ => None,
        }
    }
}

impl Drop for DnsResolver {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// # Safety
///
/// The Windows DNS API will call this function when a DNS query completes.
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
    // UNSAFETY: Take ownership of the context
    let context = unsafe { Box::from_raw(context_ptr) };

    context.active_cancel_handles.lock().remove(&context.id);

    // Process the results
    let dns_response_data = if query_results.is_null() {
        tracing::warn!(request_id = context.id, "DNS query returned null results");
        None
    } else {
        // UNSAFETY: Dereferencing raw pointer from Windows API
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
            // UNSAFETY: Create a slice from the raw response data
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
        if let Ok(fnval) = get_dns_query_raw_result_free() {
            // SAFETY: Transmute function pointer from usize
            let fnptr: unsafe extern "system" fn(*mut DNS_QUERY_RAW_RESULT) =
                unsafe { std::mem::transmute(fnval) };
            // SAFETY: Free DNS query results
            unsafe { fnptr(query_results.cast_mut()) };
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
        context.response_queue.lock().push_back(response);
    }
}
