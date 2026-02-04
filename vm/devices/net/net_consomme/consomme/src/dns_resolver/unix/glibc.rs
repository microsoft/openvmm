// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reentrant resolver backend implementation for macOS and GNU libc.

// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]

use super::DnsRequestInternal;
use super::DnsResponse;
use super::build_servfail_response;
use libc::c_int;

/// Size of the `res_state` structure for different platforms.
/// These values were derived from including resolv.h and using sizeof(struct __res_state).
#[cfg_attr(target_os = "macos")]
const RES_STATE_SIZE: usize = 552;
#[cfg_attr(target_os = "linux")]
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

/// Handle a DNS query using reentrant resolver functions (macOS and GNU libc).
pub fn handle_dns_query(request: DnsRequestInternal) {
    let mut answer = vec![0u8; 4096];
    let mut state = ResState::zeroed();

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

    #[test]
    fn test_res_ninit_and_res_nsend_callable() {
        // Test that the reentrant resolver functions are callable
        let mut state = ResState::zeroed();

        // SAFETY: res_ninit initializes the resolver state
        let init_result = unsafe { res_ninit(&mut state) };
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
            res_nsend(
                &mut state,
                dns_query.as_ptr(),
                dns_query.len() as c_int,
                answer.as_mut_ptr(),
                answer.len() as c_int,
            )
        };

        // Clean up
        // SAFETY: res_nclose frees resources associated with the resolver state.
        unsafe { res_nclose(&mut state) };
    }
}
