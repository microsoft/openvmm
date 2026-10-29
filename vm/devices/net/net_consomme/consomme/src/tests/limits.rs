// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the resource limits.

use super::*;
use crate::limits::DEFAULT_MAX_ACTIVE_ICMP_FLOWS;
use crate::limits::DEFAULT_MAX_ACTIVE_TCP_FLOWS;
use crate::limits::DEFAULT_MAX_ACTIVE_UDP_FLOWS;
use crate::limits::FlowLimits;
use crate::limits::FlowProtocol;
use crate::limits::check_flow_limit;
use std::collections::HashMap;

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
    assert_eq!(
        params.flow_limits,
        FlowLimits {
            tcp: Some(DEFAULT_MAX_ACTIVE_TCP_FLOWS),
            udp: Some(DEFAULT_MAX_ACTIVE_UDP_FLOWS),
            icmp: Some(DEFAULT_MAX_ACTIVE_ICMP_FLOWS),
        }
    );
}

#[test]
fn flow_limit_check_admits_active_flows_and_rejects_new_ones() {
    let connections = HashMap::from([(1u16, ()), (2, ())]);

    // A full flow map keeps admitting its active flows.
    assert!(check_flow_limit(FlowProtocol::Udp, &connections, Some(2), &1).is_ok());

    // A new flow is rejected with the drop reason of its protocol.
    assert!(matches!(
        check_flow_limit(FlowProtocol::Tcp, &connections, Some(2), &3),
        Err(DropReason::TcpConnectionLimit)
    ));
    assert!(matches!(
        check_flow_limit(FlowProtocol::Udp, &connections, Some(2), &3),
        Err(DropReason::UdpConnectionLimit)
    ));
    assert!(matches!(
        check_flow_limit(FlowProtocol::Icmp, &connections, Some(2), &3),
        Err(DropReason::IcmpConnectionLimit)
    ));

    // A new flow is admitted below the limit, or without one.
    assert!(check_flow_limit(FlowProtocol::Udp, &connections, Some(3), &3).is_ok());
    assert!(check_flow_limit(FlowProtocol::Udp, &connections, None, &3).is_ok());
}
