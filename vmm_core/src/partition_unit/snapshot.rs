// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition unit support for snapshots: stopping VPs at a deferred I/O
//! boundary for capture, and advancing TSC after restore downtime.

use super::PartitionRequest;
use super::PartitionUnit;
use super::PartitionUnitRunner;
use super::StopGuard;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;

/// Snapshot requests handled by the partition unit runner.
pub(super) enum SnapshotRequest {
    StopVpsAtIoBoundary(FailableRpc<(mesh::OneshotSender<()>, mesh::OneshotReceiver<()>), ()>),
    #[cfg(guest_arch = "x86_64")]
    AdvanceTsc(FailableRpc<(std::time::Duration, u64, Option<u64>), ()>),
}

impl PartitionUnit {
    /// Stops VPs after queuing stop events but before completing deferred I/O.
    ///
    /// Failure is terminal: the caller must tear down the partition instead of
    /// attempting to resume it. An I/O-completion error retains the stop reference.
    pub async fn temporarily_stop_vps_at_io_boundary(
        &mut self,
        release_io: mesh::OneshotSender<()>,
        io_completed: mesh::OneshotReceiver<()>,
    ) -> anyhow::Result<StopGuard> {
        self.req_send
            .call_failable(
                |rpc| PartitionRequest::Snapshot(SnapshotRequest::StopVpsAtIoBoundary(rpc)),
                (release_io, io_completed),
            )
            .await?;
        Ok(StopGuard(self.req_send.clone()))
    }

    /// Advances TSC state on all stopped vCPUs.
    #[cfg(guest_arch = "x86_64")]
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
            SnapshotRequest::StopVpsAtIoBoundary(rpc) => {
                rpc.handle_failable(async |(release_io, io_completed)| {
                    // Keep the stop reference on failure too: a failed
                    // boundary must not be restarted before teardown.
                    self.vp_stop_count += 1;
                    self.vp_set
                        .stop_at_io_boundary(release_io, io_completed)
                        .await
                })
                .await
            }
            #[cfg(guest_arch = "x86_64")]
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
