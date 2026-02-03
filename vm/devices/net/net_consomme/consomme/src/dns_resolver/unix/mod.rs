// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! libc resolver backend implementation.

use super::build_servfail_response;
use crate::dns_resolver::DnsBackend;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsRequestInternal;
use crate::dns_resolver::DnsResponse;
use mesh_channel_core::Sender;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))] {
        mod glibc;
        use glibc::handle_dns_query;
    } else {
        mod musl;
        use musl::handle_dns_query;
    }
}

pub struct UnixDnsResolverBackend {}

impl DnsBackend for UnixDnsResolverBackend {
    /// Execute a DNS query asynchronously using the blocking crate.
    ///
    /// Each query spawns a blocking task that uses the appropriate resolver
    /// functions for the target platform.
    fn query(&self, request: &DnsRequest<'_>, response_sender: Sender<DnsResponse>) {
        let flow = request.flow.clone();
        let query = request.dns_query.to_vec();

        blocking::unblock(move || {
            handle_dns_query(DnsRequestInternal {
                flow,
                query,
                response_sender,
            });
        })
        .detach();
    }
}

impl UnixDnsResolverBackend {
    /// Create a new DNS resolver backend.
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {})
    }
}
