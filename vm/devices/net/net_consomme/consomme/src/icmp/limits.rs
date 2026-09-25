// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Active-flow limit of ICMP source flows.

use super::Icmp;
use crate::DropReason;
use std::net::SocketAddrV4;

impl Icmp {
    /// Rejects a new guest source flow when the active-flow limit is reached,
    /// before a host socket is opened for it.
    pub(super) fn check_flow_limit(&self, guest_addr: &SocketAddrV4) -> Result<(), DropReason> {
        if !self.connections.contains_key(guest_addr)
            && self.connections.len() >= self.max_connections
        {
            tracelimit::warn_ratelimited!(
                max_connections = self.max_connections,
                guest = %guest_addr,
                "rejecting ICMP flow before host socket creation because the active-flow limit was reached"
            );
            return Err(DropReason::IcmpConnectionLimit);
        }
        Ok(())
    }
}
