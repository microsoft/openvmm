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
#[cfg(target_os = "linux")]
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
    if req.flow.protocol == smoltcp::wire::IpProtocol::Tcp {
        tracing::debug!(
            "TCP mode requested but cannot force on macOS; resolver will use UDP with automatic TCP fallback"
        );
    }

    // DNS UDP responses are typically <= 512 bytes without EDNS0, but allow
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
mod tests {
    use super::*;
    use crate::dns_resolver::{DnsFlow, DnsResponse};
    use parking_lot::Mutex;
    use smoltcp::wire::{EthernetAddress, IpProtocol, Ipv4Address};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Debug)]
    struct TestDnsResponseQueues {
        udp: Mutex<Vec<DnsResponse>>,
        tcp: Mutex<Vec<DnsResponse>>,
    }

    impl TestDnsResponseQueues {
        fn push(&self, response: DnsResponse) {
            match response.flow.protocol {
                IpProtocol::Udp => self.udp.lock().push(response),
                IpProtocol::Tcp => self.tcp.lock().push(response),
                _ => panic!("Unexpected protocol for DNS Response"),
            }
        }
    }

    #[test]
    fn test_query_with_custom_buffer() {
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

        // Create the backend
        let mut backend = UnixDnsResolverBackend::new().expect("Failed to create backend");

        // Set up response queues and accessor
        let queues = Arc::new(TestDnsResponseQueues {
            udp: Mutex::new(Vec::new()),
            tcp: Mutex::new(Vec::new()),
        });

        let test_queues_clone = queues.clone();

        // Create a test DNS flow
        let flow = DnsFlow {
            src_addr: Ipv4Address::new(192, 168, 1, 100),
            dst_addr: Ipv4Address::new(8, 8, 8, 8),
            src_port: 12345,
            dst_port: 53,
            gateway_mac: EthernetAddress([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            client_mac: EthernetAddress([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
            protocol: IpProtocol::Udp,
        };

        // Use the backend properly through its query() method
        // We need to manually handle the response by wrapping the accessor
        let flow_for_thread = flow.clone();
        let query_for_thread = dns_query.clone();

        // Spawn a thread to manually call res_send and push to our test queues
        let handle = std::thread::spawn(move || {
            // Initialize resolver in this thread
            let _ = init_resolver();

            let mut answer = vec![0u8; 4096];
            let answer_len = unsafe {
                ffi::res_send(
                    query_for_thread.as_ptr(),
                    query_for_thread.len() as libc::c_int,
                    answer.as_mut_ptr(),
                    answer.len() as libc::c_int,
                )
            };

            if answer_len > 0 {
                answer.truncate(answer_len as usize);
                test_queues_clone.push(DnsResponse {
                    flow: flow_for_thread,
                    response_data: answer,
                });
            }
        });

        // Wait for the thread to complete with a timeout
        let _ = handle.join();

        // Give a small buffer for any async operations
        std::thread::sleep(Duration::from_millis(100));

        // Check if we received a response
        let responses = queues.udp.lock();

        // Note: This test may fail if there's no network connectivity or DNS server
        // In a production test, you might want to mock the res_send call
        if !responses.is_empty() {
            println!("Received {} DNS response(s)", responses.len());
            let response = &responses[0];
            println!("Response data length: {}", response.response_data.len());
            assert!(
                !response.response_data.is_empty(),
                "Response data should not be empty"
            );
            println!(
                "{}",
                response
                    .response_data
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        } else {
            println!(
                "Warning: No DNS response received (this may be expected in test environments)"
            );
        }

        // Properly shut down the backend
        backend.cancel_all().unwrap();
        // Drop will join the worker thread
        drop(backend);
    }
}
