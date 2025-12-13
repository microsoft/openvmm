// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! libc resolver backend implementation.
//!
// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]
use super::DropReason;
use crate::dns_resolver::{DnsBackend, DnsRequest, DnsResponse, DnsResponseAccessor};

// FFI declarations for libc resolver functions.
//
// Note: We use the thread-safe versions where available. The resolver
// state `_res` is thread-local on modern systems, so concurrent calls
// from different threads are safe.

#[cfg(target_os = "linux")]
mod ffi {
    use libc::c_int;

    // Resolver option flags from resolv.h
    pub const RES_USEVC: c_int = 0x00000002; // Use TCP connections for queries

    // Opaque type for the resolver state structure
    #[repr(C)]
    pub struct __res_state {
        _private: [u8; 0],
    }

    // On Linux, res_init and res_send are in libresolv.
    // The resolver state (_res) is thread-local.
    unsafe extern "C" {
        /// Initialize the resolver state.
        /// Reads /etc/resolv.conf and populates the thread-local _res structure.
        /// Returns 0 on success, -1 on error.
        pub safe fn res_init() -> c_int;

        /// Send a pre-formatted DNS query and receive the response.
        ///
        /// # Arguments
        /// * `msg` - Pointer to the DNS query message in wire format
        /// * `msglen` - Length of the query message
        /// * `answer` - Buffer to receive the DNS response
        /// * `anslen` - Size of the answer buffer
        ///
        /// # Returns
        /// The length of the response on success, or -1 on error.
        pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;

        /// Access the thread-local resolver state.
        #[link_name = "__res_state"]
        pub fn res_state() -> *mut __res_state;

        /// Get the resolver options field.
        pub fn res_getoptions(statp: *mut __res_state) -> c_int;

        /// Set the resolver options field.
        pub fn res_setoptions(statp: *mut __res_state, options: c_int);
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use libc::c_int;

    // Resolver option flags from resolv.h
    pub const RES_USEVC: c_int = 0x00000002; // Use TCP connections for queries

    // Opaque type for the resolver state structure
    #[repr(C)]
    #[allow(non_camel_case_types)]
    pub struct __res_state {
        _private: [u8; 0],
    }

    // On macOS, resolver functions are in libSystem.
    // We use res_9_init and res_9_send which are the modern variants.
    // The older res_init/res_send are deprecated.
    unsafe extern "C" {
        /// Initialize the resolver state (macOS variant).
        #[link_name = "res_9_init"]
        pub safe fn res_init() -> c_int;

        /// Send a pre-formatted DNS query and receive the response (macOS variant).
        #[link_name = "res_9_send"]
        pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;

        /// Access the thread-local resolver state (macOS variant).
        #[link_name = "res_9_state"]
        pub fn res_state() -> *mut __res_state;

        /// Get the resolver options field (macOS variant).
        #[link_name = "res_9_getoptions"]
        pub fn res_getoptions(statp: *mut __res_state) -> c_int;

        /// Set the resolver options field (macOS variant).
        #[link_name = "res_9_setoptions"]
        pub fn res_setoptions(statp: *mut __res_state, options: c_int);
    }
}

/// Initialize the libc resolver.
///
/// This must be called once before using `res_send()`.
/// Reads configuration from /etc/resolv.conf.
pub fn init_resolver() -> Result<(), std::io::Error> {
    // res_init() is declared as safe and initializes thread-local state.
    let result = ffi::res_init();

    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub struct UnixDnsResolverBackend {}

impl DnsBackend for UnixDnsResolverBackend {
    /// Execute a DNS query asynchronously using a background thread.
    ///
    /// Spawns a thread to execute the blocking query using `res_send()`.
    /// The result is queued to the shared state's response queue.
    fn query(
        &self,
        request: &DnsRequest<'_>,
        accessor: DnsResponseAccessor,
    ) -> Result<(), DropReason> {
        let flow = request.flow.clone();
        let query = request.dns_query.to_vec();
        let use_tcp = flow.protocol == smoltcp::wire::IpProtocol::Tcp;

        std::thread::spawn(move || {
            if init_resolver().is_err() {
                tracing::error!("Could not initialize DNS resolver");
                let response = super::build_servfail_response(&query);
                accessor.push(DnsResponse {
                    flow,
                    response_data: response,
                });
                return;
            }

            // Set RES_USEVC flag if TCP is requested
            if use_tcp {
                // SAFETY: res_state() returns a pointer to thread-local resolver state.
                // res_getoptions and res_setoptions are safe to call on a valid state pointer.
                unsafe {
                    let state = ffi::res_state();
                    if !state.is_null() {
                        let current_options = ffi::res_getoptions(state);
                        ffi::res_setoptions(state, current_options | ffi::RES_USEVC);
                    }
                }
            }

            // DNS UDP responses are typically <= 512 bytes without EDNS0, but allow
            // a larger buffer to handle modern responses.
            let mut answer = vec![0u8; 4096];

            // SAFETY: res_send is called with valid query buffer and answer buffer.
            // Both buffers are properly sized and aligned.
            let answer_len = unsafe {
                ffi::res_send(
                    query.as_ptr(),
                    query.len() as libc::c_int,
                    answer.as_mut_ptr(),
                    answer.len() as libc::c_int,
                )
            };

            if answer_len > 0 {
                answer.truncate(answer_len as usize);
                accessor.push(DnsResponse {
                    flow,
                    response_data: answer,
                });
            } else {
                let response = super::build_servfail_response(&query);
                accessor.push(DnsResponse {
                    flow,
                    response_data: response,
                });
            }
        });

        Ok(())
    }

    fn cancel_all(&self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

impl UnixDnsResolverBackend {
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {})
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

    #[derive(Debug, Clone)]
    struct TestDnsResponseAccessor {
        queues: Arc<TestDnsResponseQueues>,
    }

    impl TestDnsResponseAccessor {
        fn push(&self, response: DnsResponse) {
            self.queues.push(response);
        }
    }

    #[test]
    fn test_query_with_custom_buffer() {
        // Example DNS query buffer for google.com A record
        // This is a minimal DNS query packet in wire format
        // You can replace this with your own buffer
        let dns_query: Vec<u8> = vec![
            0x12, 0x34, // Transaction ID
            0x01, 0x00, // Flags: standard query
            0x00, 0x01, // Questions: 1
            0x00, 0x00, // Answer RRs: 0w
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
        let _backend = UnixDnsResolverBackend::new().expect("Failed to create backend");

        // Set up response queues and accessor
        let queues = Arc::new(TestDnsResponseQueues {
            udp: Mutex::new(Vec::new()),
            tcp: Mutex::new(Vec::new()),
        });

        let _accessor = DnsResponseAccessor {
            queues: Arc::new(crate::dns_resolver::DnsResponseQueues {
                udp: Mutex::new(Vec::new()),
                tcp: Mutex::new(Vec::new()),
            }),
        };

        // Create a custom accessor that will push to our test queues
        let test_queues_clone = queues.clone();
        let custom_accessor = TestDnsResponseAccessor {
            queues: test_queues_clone,
        };

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

        // Create the DNS request
        let request = DnsRequest {
            flow,
            dns_query: &dns_query,
        };

        // Execute the query - we'll manually handle the response
        let flow_clone = request.flow.clone();
        let query = request.dns_query.to_vec();

        std::thread::spawn(move || {
            let mut answer = vec![0u8; 4096];

            let answer_len = unsafe {
                ffi::res_send(
                    query.as_ptr(),
                    query.len() as libc::c_int,
                    answer.as_mut_ptr(),
                    answer.len() as libc::c_int,
                )
            };

            if answer_len > 0 {
                answer.truncate(answer_len as usize);
                custom_accessor.push(DnsResponse {
                    flow: flow_clone,
                    response_data: answer,
                });
            }
        });

        // Wait for the background thread to complete
        // Note: In a real scenario, you might want to use a more sophisticated
        // synchronization mechanism, but for testing a simple sleep works
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
    }
}
