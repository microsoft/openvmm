// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reentrant resolver backend implementation for macOS and GNU libc.

// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]

use super::DnsRequestInternal;
use super::DnsResponse;
use super::build_servfail_response;
use libc::c_int;
use libc::c_ulong;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// RES_USEVC option flag - use TCP (virtual circuit) instead of UDP.
/// From glibc resolv/resolv.h: https://sourceware.org/git/?p=glibc.git;a=blob_plain;f=resolv/resolv.h;hb=HEAD
const RES_USEVC: c_ulong = 0x00000040;

/// Size of the `res_state` structure for different platforms.
/// These values were derived from including resolv.h and using sizeof(struct __res_state).
#[cfg(target_os = "macos")]
const RES_STATE_SIZE: usize = 552;
#[cfg(target_os = "linux")]
const RES_STATE_SIZE: usize = 568;

/// The prefix of the glibc `struct __res_state` that we need to access.
/// This matches the layout defined in glibc resolv/bits/types/res_state.h:
/// See: https://sourceware.org/git/?p=glibc.git;a=blob_plain;f=resolv/bits/types/res_state.h;hb=HEAD
/// See: https://github.com/apple-oss-distributions/libresolv/blob/main/resolv.h
///
/// ```c
/// struct __res_state {
///     int retrans;           /* retransmission time interval */
///     int retry;             /* number of times to retransmit */
///     unsigned long options; /* option flags */
///     ...
/// }
/// ```
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromZeros)]
struct ResStatePrefix {
    retrans: c_int,
    retry: c_int,
    options: c_ulong,
}

/// Wrapper around the glibc/macOS resolver state structure.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromZeros)]
pub struct ResState {
    prefix: ResStatePrefix,
    _rest: [u8; RES_STATE_SIZE - size_of::<ResStatePrefix>()],
}

impl ResState {
    /// Set the options field in the resolver state.
    pub fn set_options(&mut self, options: c_ulong) {
        self.prefix.options = options;
    }

    /// Get the options field from the resolver state.
    pub fn options(&self) -> c_ulong {
        self.prefix.options
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

/// Handle a DNS query using reentrant resolver functions (macOS and GNU libc).
pub fn handle_dns_query(request: DnsRequestInternal) {
    let mut answer = vec![0u8; 4096];
    let mut state = ResState::new_zeroed();

    // SAFETY: res_ninit initializes the resolver state by reading /etc/resolv.conf.
    // The state is properly sized and aligned.
    let result = unsafe { res_ninit(&mut state) };
    if result == -1 {
        tracing::error!("res_ninit failed, returning SERVFAIL");
        let response = build_servfail_response(&request.query);
        request.response_sender.send(DnsResponse {
            flow: request.flow,
            response_data: response,
        });
        return;
    }

    // Set RES_USEVC to force TCP for DNS queries.
    if request.flow.transport == crate::dns_resolver::DnsTransport::Tcp {
        state.set_options(state.options() | RES_USEVC);
    }
    // SAFETY: res_nsend is called with valid state, query buffer and answer buffer.
    // All buffers are properly sized and aligned. The state was initialized above.
    let answer_len = unsafe {
        res_nsend(
            &mut state,
            request.query.as_ptr(),
            request.query.len() as c_int,
            answer.as_mut_ptr(),
            answer.len() as c_int,
        )
    };

    // SAFETY: res_nclose frees resources associated with the resolver state.
    // The state was initialized by res_ninit above.
    unsafe { res_nclose(&mut state) };

    if answer_len > 0 {
        answer.truncate(answer_len as usize);
        request.response_sender.send(DnsResponse {
            flow: request.flow,
            response_data: answer,
        });
    } else {
        tracing::error!("DNS query failed, returning SERVFAIL");
        let response = build_servfail_response(&request.query);
        request.response_sender.send(DnsResponse {
            flow: request.flow,
            response_data: response,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Example DNS query buffer for google.com A record.
    fn sample_dns_query() -> Vec<u8> {
        vec![
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
        ]
    }

    /// RAII wrapper for ResState that ensures proper cleanup.
    struct InitializedResState {
        state: ResState,
    }

    impl InitializedResState {
        fn new() -> Self {
            let mut state = ResState::new_zeroed();
            // SAFETY: res_ninit initializes the resolver state
            let result = unsafe { res_ninit(&mut state) };
            assert_eq!(result, 0, "res_ninit() should succeed");
            Self { state }
        }

        /// Send a DNS query and return the response length.
        fn send_query(&mut self, query: &[u8]) -> c_int {
            let mut answer = vec![0u8; 4096];
            // SAFETY: res_nsend is called with valid state, query buffer and answer buffer.
            unsafe {
                res_nsend(
                    &mut self.state,
                    query.as_ptr(),
                    query.len() as c_int,
                    answer.as_mut_ptr(),
                    answer.len() as c_int,
                )
            }
        }
    }

    impl Drop for InitializedResState {
        fn drop(&mut self) {
            // SAFETY: res_nclose frees resources associated with the resolver state.
            unsafe { res_nclose(&mut self.state) };
        }
    }

    #[test]
    fn test_res_ninit_and_res_nsend_callable() {
        let mut state = InitializedResState::new();
        let _answer_len = state.send_query(&sample_dns_query());
    }

    #[test]
    fn test_res_usevc_flag_for_tcp() {
        let mut state = InitializedResState::new();

        // Verify we can read and modify the options field
        let original_options = state.state.options();
        state.state.set_options(original_options | RES_USEVC);
        assert_ne!(
            state.state.options() & RES_USEVC,
            0,
            "RES_USEVC flag should be set"
        );

        // With RES_USEVC set, this should use TCP instead of UDP.
        let _answer_len = state.send_query(&sample_dns_query());
    }
}
