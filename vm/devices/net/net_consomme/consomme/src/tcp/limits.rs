// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Active-flow limit of TCP connections.

use super::Tcp;
use crate::DropReason;
use crate::FourTuple;

impl Tcp {
    /// Rejects a new guest flow when the active-flow limit is reached, before
    /// a host socket is created for it.
    pub(super) fn check_flow_limit(&self, ft: &FourTuple) -> Result<(), DropReason> {
        if !self.connections.contains_key(ft) && self.connections.len() >= self.max_connections {
            tracelimit::warn_ratelimited!(
                max_connections = self.max_connections,
                src = %ft.src,
                dst = %ft.dst,
                "rejecting TCP flow before host socket creation because the active-flow limit was reached"
            );
            return Err(DropReason::TcpConnectionLimit);
        }
        Ok(())
    }
}
