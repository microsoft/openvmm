// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! libc resolver backend implementation.
//!
// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]
use super::DnsRequestInternal;
use super::DropReason;
use super::build_servfail_response;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
use crate::dns_resolver::DnsResponseAccessor;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

// FFI declarations for libc resolver functions.
// Only available on glibc-based Linux (musl doesn't have libresolv).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod ffi {
    use libc::c_int;

    unsafe extern "C" {

        #[link_name = "__res_init"]
        pub fn res_init() -> c_int;

        pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use libc::c_int;
    // On macOS, resolver functions are in libSystem.
    // We use res_9_init and res_9_send.
    unsafe extern "C" {
        #[link_name = "res_9_init"]
        pub fn res_init() -> c_int;

        #[link_name = "res_9_send"]
        pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;
    }
}
/// Initialize the libc resolver.
///
/// This must be called once before using `res_send()`.
pub fn init_resolver() -> Result<(), std::io::Error> {
    // SAFETY: res_init() initializes thread-local resolver state by reading
    // /etc/resolv.conf. It is safe to call concurrently from different threads
    // since the resolver state is thread-local
    let result = unsafe { ffi::res_init() };

    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub struct UnixDnsResolverBackend {
    worker: Option<std::thread::JoinHandle<()>>,
    request_tx: Option<std::sync::mpsc::Sender<DnsRequestInternal>>,
    shutdown: Arc<AtomicBool>,
}

impl DnsBackend for UnixDnsResolverBackend {
    /// Execute a DNS query asynchronously using the worker thread.
    ///
    /// Sends the request to the worker thread via a channel. The worker will
    /// process it asynchronously and return the response via the accessor.
    fn query(
        &self,
        request: &DnsRequest<'_>,
        accessor: DnsResponseAccessor,
    ) -> Result<(), DropReason> {
        let internal_request = DnsRequestInternal {
            flow: request.flow.clone(),
            query: request.dns_query.to_vec(),
            accessor,
        };

        // Try to send the request to the worker thread
        // If the channel is closed (worker shut down), drop silently
        if let Some(ref tx) = self.request_tx {
            let _ = tx.send(internal_request);
        }

        Ok(())
    }

    fn cancel_all(&mut self) -> Result<(), std::io::Error> {
        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);

        // Close the channel by dropping the sender
        // This will cause the worker's receive loop to exit
        self.request_tx.take();

        Ok(())
    }
}

impl UnixDnsResolverBackend {
    /// Create a new DNS resolver backend.
    ///
    /// On glibc Linux and macOS, this uses the system's libresolv library.
    /// On musl Linux, libresolv is not available, so this returns an error
    /// and the caller should fall back to DHCP-based DNS settings.
    pub fn new() -> Result<Self, std::io::Error> {
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Create a dedicated blocking worker thread for DNS resolution
        let worker = std::thread::Builder::new()
            .name("dns-worker".to_string())
            .spawn(move || {
                // Initialize the resolver once at thread startup
                if let Err(e) = init_resolver() {
                    tracing::error!("Failed to initialize DNS resolver: {}, worker exiting", e);
                    return;
                }

                // Process DNS requests sequentially in this thread
                while let Ok(req) = request_rx.recv() {
                    // Check for shutdown signal
                    if shutdown_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    // Handle the DNS query synchronously in this worker thread
                    handle_dns_query(req);
                }

                tracing::debug!("DNS worker thread exiting");
            })?;

        Ok(Self {
            worker: Some(worker),
            request_tx: Some(request_tx),
            shutdown,
        })
    }
}

/// Handle a single DNS query using the blocking res_send() function.
///
/// This function is called sequentially by the worker thread.
/// The resolver state has already been initialized via res_init() at thread startup.
fn handle_dns_query(req: DnsRequestInternal) {
    // DNS responses are typically <= 512 bytes without EDNS0, but allow
    // a larger buffer to handle modern responses.
    let mut answer = vec![0u8; 4096];

    // SAFETY: res_send is called with valid query buffer and answer buffer.
    // Both buffers are properly sized and aligned. The query slice is valid
    // for the duration of the call, and the answer buffer is owned by this thread.
    let answer_len = unsafe {
        ffi::res_send(
            req.query.as_ptr(),
            req.query.len() as libc::c_int,
            answer.as_mut_ptr(),
            answer.len() as libc::c_int,
        )
    };

    if answer_len > 0 {
        answer.truncate(answer_len as usize);
        req.accessor.push(DnsResponse {
            flow: req.flow,
            response_data: answer,
        });
    } else {
        tracing::error!("DNS query failed, returning SERVFAIL");
        let response = build_servfail_response(&req.query);
        req.accessor.push(DnsResponse {
            flow: req.flow,
            response_data: response,
        });
    }
}

impl Drop for UnixDnsResolverBackend {
    fn drop(&mut self) {
        // Signal shutdown and close the channel
        let _ = self.cancel_all();

        // Wait for the worker thread to finish
        // The thread will exit when the channel is closed and all tasks complete
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
mod tests {
    use super::*;

    #[test]
    fn test_init_resolver_and_res_send_callable() {
        // Test that init_resolver() is callable without failure
        init_resolver().expect("init_resolver() should succeed");

        // Example DNS query buffer for google.com A record
        // This is a minimal DNS query packet in wire format
        let dns_query: Vec<u8> = vec![
            0x12, 0x34, // Transaction ID
            0x01, 0x00, // Flags: standard query
            0x00, 0x01, // Questions: 1
            0x00, 0x00, // Answer RRs: 0
            0x00, 0x00, // Authority RRs: 0
            0x00, 0x00, // Additional RRs: 0
            // Query: google.com
            0x06, 0x67, 0x6f, 0x6f, 0x67, 0x6c, 0x65, // "google"
            0x03, 0x63, 0x6f, 0x6d, // "com"
            0x00, // null terminator
            0x00, 0x01, // Type: A
            0x00, 0x01, // Class: IN
        ];

        let mut answer = vec![0u8; 4096];

        // Test that ffi::res_send is callable without crashing.
        // The return value may be negative if there's no network connectivity,
        // but the function should not panic or cause undefined behavior.
        // SAFETY: res_send is called with valid query buffer and answer buffer.
        let _answer_len = unsafe {
            ffi::res_send(
                dns_query.as_ptr(),
                dns_query.len() as libc::c_int,
                answer.as_mut_ptr(),
                answer.len() as libc::c_int,
            )
        };

        // We don't assert on the result since it depends on network connectivity.
        // The test passes as long as the functions are callable without panicking.
    }
}
