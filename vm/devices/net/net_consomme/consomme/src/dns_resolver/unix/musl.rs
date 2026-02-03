// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Global resolver backend implementation for MUSL libc.

// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]

use super::DnsRequestInternal;
use super::DnsResponse;
use super::build_servfail_response;
use libc::c_int;

unsafe extern "C" {
    pub fn res_send(msg: *const u8, msglen: c_int, answer: *mut u8, anslen: c_int) -> c_int;
}

/// Handle a DNS query using global resolver functions (MUSL libc).
pub fn handle_dns_query(request: DnsRequestInternal) {
    let mut answer = vec![0u8; 4096];

    // SAFETY: res_send is called with valid query buffer and answer buffer.
    // All buffers are properly sized and aligned.
    let answer_len = unsafe {
        res_send(
            request.query.as_ptr(),
            request.query.len() as libc::c_int,
            answer.as_mut_ptr(),
            answer.len() as libc::c_int,
        )
    };

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
    fn test_res_init_and_res_send_callable() {
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
            res_send(
                dns_query.as_ptr(),
                dns_query.len() as libc::c_int,
                answer.as_mut_ptr(),
                answer.len() as libc::c_int,
            )
        };
    }
}
