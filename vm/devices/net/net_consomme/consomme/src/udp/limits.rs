// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Active-flow limit of UDP flows.

use super::Udp;
use crate::DropReason;
use std::net::SocketAddr;

impl Udp {
    /// Rejects a new guest flow when the active-flow limit is reached, before
    /// a host socket is bound for it.
    pub(super) fn check_flow_limit(&self, guest_addr: &SocketAddr) -> Result<(), DropReason> {
        if !self.connections.contains_key(guest_addr)
            && self.connections.len() >= self.max_connections
        {
            tracelimit::warn_ratelimited!(
                max_connections = self.max_connections,
                guest = %guest_addr,
                "rejecting UDP flow before host socket binding because the active-flow limit was reached"
            );
            return Err(DropReason::UdpConnectionLimit);
        }
        Ok(())
    }
}
