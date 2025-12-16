// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! libc resolver backend implementation.
//!
// UNSAFETY: FFI calls to libc resolver functions.
#![expect(unsafe_code)]
use super::DropReason;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
use crate::dns_resolver::DnsResponseAccessor;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

pub struct WindowsDnsResolverBackend {}

#[derive(Debug)]
struct DnsRequestInternal {
    flow: super::DnsFlow,
    query: Vec<u8>,
    accessor: DnsResponseAcessor,
}

impl DnsBackend for WindowsDnsResolverBackend {
    fn new() -> Self {
        WindowsDnsResolverBackend {}
    }

    fn resolve(
        &self,
        flow: super::DnsFlow,
        query: Vec<u8>,
        accessor: DnsResponseAccessor,
    ) -> Arc<DnsRequest> {
        Arc::new(DnsRequestInternal {
            flow,
            query,
            accessor,
        })
    }
}

impl DnsRequestInternal {
    fn resolve(&self) {
        // Implementation for resolving DNS requests on Windows
    }
}

impl Drop for DnsRequestInternal {
    fn drop(&mut self) {
        // Implementation for dropping DNS requests on Windows
    }
}
