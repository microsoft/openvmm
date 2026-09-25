// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition unit support for snapshots: advancing TSC after restore downtime.

use super::PartitionRequest;
use super::PartitionUnit;
use super::PartitionUnitRunner;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;

/// Snapshot requests handled by the partition unit runner.
pub(super) enum SnapshotRequest {
    AdvanceTsc(FailableRpc<(std::time::Duration, u64, Option<u64>), ()>),
}

impl PartitionUnit {
    /// Advances TSC state on all stopped vCPUs.
    pub async fn advance_tsc(
        &mut self,
        duration: std::time::Duration,
        frequency_hz: u64,
        apic_frequency_hz: Option<u64>,
    ) -> anyhow::Result<()> {
        self.req_send
            .call_failable(
                |rpc| PartitionRequest::Snapshot(SnapshotRequest::AdvanceTsc(rpc)),
                (duration, frequency_hz, apic_frequency_hz),
            )
            .await?;
        Ok(())
    }
}

impl PartitionUnitRunner {
    /// Handles a snapshot request from [`PartitionUnit`].
    pub(super) async fn handle_snapshot(&mut self, request: SnapshotRequest) {
        match request {
            SnapshotRequest::AdvanceTsc(rpc) => {
                rpc.handle_failable(async |(duration, frequency_hz, apic_frequency_hz)| {
                    self.vp_set
                        .advance_tsc(duration, frequency_hz, apic_frequency_hz)
                        .await
                })
                .await
            }
        }
    }
}
