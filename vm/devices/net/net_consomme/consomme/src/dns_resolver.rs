// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS resolver using Windows DNS Raw APIs.
//!
//! This module provides a Rust wrapper around the Windows DNS Raw APIs
//! (DnsQueryRaw, DnsCancelQueryRaw, DnsQueryRawResultFree) that allow
//! for raw DNS query processing similar to the WSL DnsResolver implementation.

// #![expect(unsafe_code)]

// use std::sync::atomic::{AtomicBool, Ordering};
// use std::sync::Once;
// use thiserror::Error;
// use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, WIN32_ERROR, HMODULE};
// use windows_sys::Win32::NetworkManagement::Dns::{
//     DNS_PROTOCOL_TCP, DNS_PROTOCOL_UDP, DNS_QUERY_NO_MULTICAST, DNS_QUERY_RAW_CANCEL,
//     DNS_QUERY_RAW_REQUEST, DNS_QUERY_RAW_REQUEST_VERSION1, DNS_QUERY_RAW_RESULTS_VERSION1,
//     DNS_QUERY_RAW_RESULT,
// };
// use windows_sys::core::PCWSTR;

// mod test {

//     #[allow(unused_imports)]
//     use super::*;
//     #[test]
//     fn test_dns_resolver_compile() {
//         let dnsQueryRaw = "83 7d 01 00 00 01 00 00 00 00 00 00 06 67 6c 6f 62 61 6c 0f 6c 69 76 65 64 69 61 67 6e 6f 73 74 69 63 73 07 6d 6f 6e 69 74 6f 72 05 61 7a 75 72 65 03 63 6f 6d 00 00 1c 00 01";
//         //Convert the hex string to a byte vector
//         let dnsQueryRaw = dnsQueryRaw
//             .split(' ')
//             .map(|s| u8::from_str_radix(s, 16).unwrap())
//             .collect::<Vec<u8>>();

//         let request = DNS_QUERY_RAW_REQUEST {
//             version: DNS_QUERY_RAW_REQUEST_VERSION1,
//             resultsVersion: DNS_QUERY_RAW_RESULTS_VERSION1,
//             dnsQueryRawSize: dnsQueryRaw.len() as u32,
//             dnsQueryRaw: dnsQueryRaw.as_mut_ptr(),
//             dnsQueryName: PCWSTR::default().into(),
//             dnsQueryType: 0,
//             interfaceIndex: 0,

//         }
//     }
// }
