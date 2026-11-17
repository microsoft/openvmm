// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Limits on the number of simultaneously active guest flows.
//!
//! Each bounded protocol rejects a new guest flow once it has as many active
//! flows as its limit, before creating any host socket for it. An unbounded
//! protocol admits any number of flows.

use crate::DropReason;
use inspect::Inspect;
use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;

/// Default maximum number of simultaneously active guest TCP flows.
///
/// The limit is checked before creating a host TCP socket or allocating
/// per-flow TCP buffers. A connection lingering in TIME-WAIT counts toward the
/// limit only until a new flow needs its slot.
pub const DEFAULT_MAX_ACTIVE_TCP_FLOWS: usize = 128;
/// Default maximum number of simultaneously active guest UDP flows.
///
/// The limit is checked before binding a host UDP socket.
pub const DEFAULT_MAX_ACTIVE_UDP_FLOWS: usize = 256;
/// Default maximum number of simultaneously active guest ICMP source flows.
///
/// The limit is checked before opening a host ICMP socket.
pub const DEFAULT_MAX_ACTIVE_ICMP_FLOWS: usize = 16;

/// Per-protocol limits on the number of simultaneously active guest flows.
///
/// `None` leaves a protocol unbounded. Lift a limit only for a guest that is
/// trusted not to exhaust host sockets and memory.
#[derive(Inspect, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowLimits {
    /// Maximum number of active guest TCP flows.
    pub tcp: Option<usize>,
    /// Maximum number of active guest UDP flows.
    pub udp: Option<usize>,
    /// Maximum number of active guest ICMP source flows.
    pub icmp: Option<usize>,
}

impl FlowLimits {
    /// Leaves every protocol unbounded, for a trusted guest.
    pub const UNBOUNDED: Self = Self {
        tcp: None,
        udp: None,
        icmp: None,
    };
}

impl Default for FlowLimits {
    /// Bounds each protocol by its `DEFAULT_MAX_ACTIVE_*_FLOWS` limit.
    fn default() -> Self {
        Self {
            tcp: Some(DEFAULT_MAX_ACTIVE_TCP_FLOWS),
            udp: Some(DEFAULT_MAX_ACTIVE_UDP_FLOWS),
            icmp: Some(DEFAULT_MAX_ACTIVE_ICMP_FLOWS),
        }
    }
}

/// A protocol whose active guest flows are limited.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FlowProtocol {
    Tcp,
    Udp,
    Icmp,
}

/// Rejects the new guest flow `flow` of `protocol` when `connections` has
/// reached `limit`, before a host socket is created for it. A flow already in
/// `connections` is always admitted.
pub(crate) fn check_flow_limit<K: Eq + Hash + Display, V>(
    protocol: FlowProtocol,
    connections: &HashMap<K, V>,
    limit: Option<usize>,
    flow: &K,
) -> Result<(), DropReason> {
    if !connections.contains_key(flow)
        && let Some(max_connections) = reached(limit, connections.len())
    {
        return Err(reject(protocol, max_connections, flow));
    }
    Ok(())
}

/// Reports that the new guest flow `flow` of `protocol` was rejected at the
/// active-flow limit `max_connections`, and returns the reason to drop it.
pub(crate) fn reject(
    protocol: FlowProtocol,
    max_connections: usize,
    flow: &dyn Display,
) -> DropReason {
    tracelimit::warn_ratelimited!(
        ?protocol,
        max_connections,
        %flow,
        "rejecting guest flow before host socket creation because the active-flow limit was reached"
    );
    match protocol {
        FlowProtocol::Tcp => DropReason::TcpConnectionLimit,
        FlowProtocol::Udp => DropReason::UdpConnectionLimit,
        FlowProtocol::Icmp => DropReason::IcmpConnectionLimit,
    }
}

/// Returns `limit` if it is set and `active` flows have reached it.
pub(crate) fn reached(limit: Option<usize>, active: usize) -> Option<usize> {
    limit.filter(|&max| active >= max)
}
