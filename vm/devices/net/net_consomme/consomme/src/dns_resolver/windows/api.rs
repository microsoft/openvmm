// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Raw DNS API bindings with delay-loading support.

// Ensure winapi dependency is recognized as used (required for pal::delayload!)
use winapi as _;

use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_CANCEL;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_REQUEST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_RESULT;

pal::delayload! {"dnsapi.dll" {
    pub fn DnsQueryRaw(
        request: *const DNS_QUERY_RAW_REQUEST,
        cancel: *mut DNS_QUERY_RAW_CANCEL
    ) -> i32;

    pub fn DnsCancelQueryRaw(
        cancel: *const DNS_QUERY_RAW_CANCEL
    ) -> i32;

    pub fn DnsQueryRawResultFree(
        result: *mut DNS_QUERY_RAW_RESULT
    ) -> ();
}}
