// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the default resource limits.

use super::*;
use crate::limits::DEFAULT_MAX_ACTIVE_ICMP_FLOWS;
use crate::limits::DEFAULT_MAX_ACTIVE_TCP_FLOWS;
use crate::limits::DEFAULT_MAX_ACTIVE_UDP_FLOWS;

#[test]
fn default_resource_limits_are_pinned() {
    let params = ConsommeParams::new().unwrap();
    assert_eq!(params.udp_timeout, Duration::from_secs(300));
    assert_eq!(
        params.tcp_rx_buffer,
        TcpBufferBounds {
            initial: 16 << 10,
            max: 4 << 20,
        }
    );
    assert_eq!(
        params.tcp_tx_buffer,
        TcpBufferBounds {
            initial: 16 << 10,
            max: 4 << 20,
        }
    );
    assert_eq!(DEFAULT_MAX_ACTIVE_TCP_FLOWS, 128);
    assert_eq!(DEFAULT_MAX_ACTIVE_UDP_FLOWS, 256);
    assert_eq!(dns_resolver::DEFAULT_MAX_PENDING_DNS_REQUESTS, 256);
    assert_eq!(DEFAULT_MAX_ACTIVE_ICMP_FLOWS, 16);
}
