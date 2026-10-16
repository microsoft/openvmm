// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Active-flow limit of TCP connections.

use super::ConnectionCloseReason;
use super::Tcp;
use super::TcpAggregateStats;
use super::TcpBackend;
use super::TcpConnection;
use super::TcpState;
use crate::DropReason;
use crate::FourTuple;
use std::collections::HashMap;

impl Tcp {
    /// Rejects a new guest flow when the active-flow limit is reached and no
    /// connection in TIME-WAIT can make room for it, before a host socket is
    /// created for the new flow.
    pub(super) fn check_flow_limit(&mut self, ft: &FourTuple) -> Result<(), DropReason> {
        if let Some(max_connections) = make_room(
            &mut self.connections,
            self.max_connections,
            ft,
            &mut self.aggregate_stats,
        ) {
            tracelimit::warn_ratelimited!(
                max_connections,
                src = %ft.src,
                dst = %ft.dst,
                "rejecting TCP flow before host socket creation because the active-flow limit was reached"
            );
            return Err(DropReason::TcpConnectionLimit);
        }
        Ok(())
    }
}

/// Makes room for the new flow `ft` when `connections` has reached `limit`,
/// by reclaiming the connection in TIME-WAIT that is closest to expiry.
/// Returns the limit if `ft` cannot be admitted.
///
/// A connection in TIME-WAIT is closed in both directions and only lingers to
/// absorb late segments, so a new flow needs its slot more. The search is
/// linear in the limit, but it runs only while the map is full.
pub(super) fn make_room(
    connections: &mut HashMap<FourTuple, TcpConnection>,
    limit: Option<usize>,
    ft: &FourTuple,
    stats: &mut TcpAggregateStats,
) -> Option<usize> {
    if connections.contains_key(ft) {
        return None;
    }
    let max_connections = crate::limits::reached(limit, connections.len())?;
    let Some(reclaimed) = connections
        .iter()
        .filter(|(_, conn)| conn.is_reclaimable())
        .min_by_key(|(_, conn)| conn.inner.lifetime_timer.deadline())
        .map(|(flow, _)| *flow)
    else {
        return Some(max_connections);
    };
    connections.remove(&reclaimed);
    tracing::debug!(
        src = %reclaimed.src,
        dst = %reclaimed.dst,
        "reclaiming TIME-WAIT connection to admit a new flow at the active-flow limit"
    );
    stats.record_close(ConnectionCloseReason::Normal);
    stats.connections_reclaimed_time_wait.increment();
    None
}

impl TcpConnection {
    /// Whether the connection only lingers in TIME-WAIT and has released its
    /// host socket, which happens once both directions are closed and nothing
    /// is left to write to it, so that dropping it early loses no data.
    fn is_reclaimable(&self) -> bool {
        self.inner.state == TcpState::TimeWait
            && matches!(self.backend, TcpBackend::Socket { socket: None, .. })
    }
}
