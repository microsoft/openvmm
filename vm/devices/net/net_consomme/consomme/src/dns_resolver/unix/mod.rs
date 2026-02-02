// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! libc resolver backend implementation.

// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]
use super::DropReason;
use super::build_servfail_response;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsRequestInternal;
use crate::dns_resolver::DnsResponse;
use crate::dns_resolver::DnsResponseAccessor;

mod ffi {
    use libc::c_int;

    cfg_if::cfg_if! {
        if #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))] {
            // Reentrant resolver functions for macOS and GNU libc

            const RES_STATE_SIZE: usize = 568;

            #[repr(C)]
            pub struct ResState {
                _data: [u8; RES_STATE_SIZE],
            }

            impl ResState {
                pub fn zeroed() -> Self {
                    Self {
                        _data: [0u8; RES_STATE_SIZE],
                    }
                }
            }

            unsafe extern "C" {
                #[cfg_attr(target_os = "macos", link_name = "res_9_ninit")]
                #[cfg_attr(
                    all(target_os = "linux", target_env = "gnu"),
                    link_name = "__res_ninit"
                )]
                pub fn res_ninit(statep: *mut ResState) -> c_int;

                #[cfg_attr(target_os = "macos", link_name = "res_9_nsend")]
                pub fn res_nsend(
                    statep: *mut ResState,
                    msg: *const u8,
                    msglen: c_int,
                    answer: *mut u8,
                    anslen: c_int,
                ) -> c_int;

                #[cfg_attr(target_os = "macos", link_name = "res_9_nclose")]
                #[cfg_attr(
                    all(target_os = "linux", target_env = "gnu"),
                    link_name = "__res_nclose"
                )]
                pub fn res_nclose(statep: *mut ResState);
            }
        } else {
            // Global resolver functions for MUSL libc

            unsafe extern "C" {
                pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;
            }
        }
    }
}

pub struct UnixDnsResolverBackend {}

impl DnsBackend for UnixDnsResolverBackend {
    /// Execute a DNS query asynchronously using the blocking crate.
    ///
    /// Each query spawns a blocking task that uses the appropriate resolver
    /// functions for the target platform.
    fn query(
        &self,
        request: &DnsRequest<'_>,
        accessor: DnsResponseAccessor,
    ) -> Result<(), DropReason> {
        let flow = request.flow.clone();
        let query = request.dns_query.to_vec();

        blocking::unblock(move || {
            handle_dns_query(DnsRequestInternal {
                flow,
                query,
                accessor,
            });
        })
        .detach();

        Ok(())
    }

    fn cancel_all(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

impl UnixDnsResolverBackend {
    /// Create a new DNS resolver backend.
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {})
    }
}

impl Drop for UnixDnsResolverBackend {
    fn drop(&mut self) {
        let _ = self.cancel_all();
    }
}

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))] {
        /// Handle a DNS query using reentrant resolver functions (macOS and GNU libc).
        fn handle_dns_query(request: DnsRequestInternal) {
            let mut answer = vec![0u8; 4096];
            let mut state = ffi::ResState::zeroed();

            // SAFETY: res_ninit initializes the resolver state by reading /etc/resolv.conf.
            // The state is properly sized and aligned.
            let result = unsafe { ffi::res_ninit(&mut state) };
            if result == -1 {
                tracing::error!("res_ninit failed, returning SERVFAIL");
                let response = build_servfail_response(&request.query);
                request.accessor.push(DnsResponse {
                    flow: request.flow,
                    response_data: response,
                });
                return;
            }

            // SAFETY: res_nsend is called with valid state, query buffer and answer buffer.
            // All buffers are properly sized and aligned. The state was initialized above.
            let answer_len = unsafe {
                ffi::res_nsend(
                    &mut state,
                    request.query.as_ptr(),
                    request.query.len() as libc::c_int,
                    answer.as_mut_ptr(),
                    answer.len() as libc::c_int,
                )
            };

            // SAFETY: res_nclose frees resources associated with the resolver state.
            // The state was initialized by res_ninit above.
            unsafe { ffi::res_nclose(&mut state) };

            if answer_len > 0 {
                answer.truncate(answer_len as usize);
                request.accessor.push(DnsResponse {
                    flow: request.flow,
                    response_data: answer,
                });
            } else {
                tracing::error!("DNS query failed, returning SERVFAIL");
                let response = build_servfail_response(&request.query);
                request.accessor.push(DnsResponse {
                    flow: request.flow,
                    response_data: response,
                });
            }
        }
    } else {
        /// Handle a DNS query using global resolver functions (MUSL libc).
        fn handle_dns_query(request: DnsRequestInternal) {
            let mut answer = vec![0u8; 4096];

            // SAFETY: res_send is called with valid query buffer and answer buffer.
            // All buffers are properly sized and aligned.
            let answer_len = unsafe {
                ffi::res_send(
                    request.query.as_ptr(),
                    request.query.len() as libc::c_int,
                    answer.as_mut_ptr(),
                    answer.len() as libc::c_int,
                )
            };

            if answer_len > 0 {
                answer.truncate(answer_len as usize);
                request.accessor.push(DnsResponse {
                    flow: request.flow,
                    response_data: answer,
                });
            } else {
                tracing::error!("DNS query failed, returning SERVFAIL");
                let response = build_servfail_response(&request.query);
                request.accessor.push(DnsResponse {
                    flow: request.flow,
                    response_data: response,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    cfg_if::cfg_if! {
        if #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))] {
            #[test]
            fn test_res_ninit_and_res_nsend_callable() {
                // Test that the reentrant resolver functions are callable
                let mut state = ffi::ResState::zeroed();

                // SAFETY: res_ninit initializes the resolver state
                let init_result = unsafe { ffi::res_ninit(&mut state) };
                assert_eq!(init_result, 0, "res_ninit() should succeed");

                // Example DNS query buffer for google.com A record
                let dns_query: Vec<u8> = vec![
                    0x12, 0x34, // Transaction ID
                    0x01, 0x00, // Flags: standard query
                    0x00, 0x01, // Questions: 1
                    0x00, 0x00, // Answer RRs: 0
                    0x00, 0x00, // Authority RRs: 0
                    0x00, 0x00, // Additional RRs: 0
                    0x06, 0x67, 0x6f, 0x6f, 0x67, 0x6c, 0x65, 0x03, 0x63, 0x6f, 0x6d,
                    0x00, // null terminator
                    0x00, 0x01, // Type: A
                    0x00, 0x01, // Class: IN
                ];

                let mut answer = vec![0u8; 4096];

                // SAFETY: res_nsend is called with valid state, query buffer and answer buffer.
                let _answer_len = unsafe {
                    ffi::res_nsend(
                        &mut state,
                        dns_query.as_ptr(),
                        dns_query.len() as libc::c_int,
                        answer.as_mut_ptr(),
                        answer.len() as libc::c_int,
                    )
                };

                // Clean up
                // SAFETY: res_nclose frees resources associated with the resolver state.
                unsafe { ffi::res_nclose(&mut state) };
            }
        } else {
            #[test]
            fn test_res_init_and_res_send_callable() {
                // SAFETY: res_init() initializes global resolver state by reading /etc/resolv.conf.
                let result = unsafe { ffi::res_init() };
                assert_ne!(result, -1, "res_init() should succeed");

                // Example DNS query buffer for google.com A record
                let dns_query: Vec<u8> = vec![
                    0x12, 0x34, // Transaction ID
                    0x01, 0x00, // Flags: standard query
                    0x00, 0x01, // Questions: 1
                    0x00, 0x00, // Answer RRs: 0
                    0x00, 0x00, // Authority RRs: 0
                    0x00, 0x00, // Additional RRs: 0
                    0x06, 0x67, 0x6f, 0x6f, 0x67, 0x6c, 0x65, 0x03, 0x63, 0x6f, 0x6d,
                    0x00, // null terminator
                    0x00, 0x01, // Type: A
                    0x00, 0x01, // Class: IN
                ];

                let mut answer = vec![0u8; 4096];

                // SAFETY: res_send is called with valid query buffer and answer buffer.
                let _answer_len = unsafe {
                    ffi::res_send(
                        dns_query.as_ptr(),
                        dns_query.len() as libc::c_int,
                        answer.as_mut_ptr(),
                        answer.len() as libc::c_int,
                    )
                };
            }
        }
    }
}
