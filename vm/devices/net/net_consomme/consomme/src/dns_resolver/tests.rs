// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the DNS resolver's pending-request limit.

use super::*;

struct HoldingBackend;

impl DnsBackend for HoldingBackend {
    fn query(
        &self,
        _request: &DnsRequest<'_>,
        _response_sender: Sender<DnsResponse>,
        _query_id: u64,
    ) {
    }
}

fn request() -> DnsRequest<'static> {
    static QUERY: [u8; DNS_HEADER_SIZE + 1] = [0; DNS_HEADER_SIZE + 1];
    DnsRequest {
        flow: DnsFlow {
            src: "10.0.0.2:10000".parse().unwrap(),
            dst: "10.0.0.1:53".parse().unwrap(),
            gateway_mac: EthernetAddress([0x52, 0x54, 0, 0, 0, 1]),
            client_mac: EthernetAddress([0x52, 0x54, 0, 0, 0, 2]),
            transport: DnsTransport::Udp,
        },
        dns_query: &QUERY,
    }
}

#[test]
fn default_pending_request_limit_returns_servfail() {
    let mut resolver = DnsResolver::new_for_test(Arc::new(HoldingBackend));
    let request = request();
    for _ in 0..DEFAULT_MAX_PENDING_DNS_REQUESTS {
        assert!(resolver.submit_udp_query(&request).unwrap().is_none());
    }

    let response = resolver
        .submit_udp_query(&request)
        .unwrap()
        .expect("the pending-request limit must return SERVFAIL");
    assert_eq!(resolver.pending_requests, DEFAULT_MAX_PENDING_DNS_REQUESTS);
    assert_eq!(response.response_data[3] & 0x0f, 2);
}
