// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition unit support for snapshots: validating the instantiated VP prefix,
//! stopping VPs at a deferred I/O boundary for capture, and the NVX time ABI's
//! LAPIC timer checks and restore advance.

use super::Error;
use super::PartitionRequest;
use super::PartitionUnit;
use super::PartitionUnitParams;
use super::PartitionUnitRunner;
use super::StopGuard;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;

/// Snapshot requests handled by the partition unit runner.
pub(super) enum SnapshotRequest {
    StopVpsAtIoBoundary(FailableRpc<(mesh::OneshotSender<()>, mesh::OneshotReceiver<()>), ()>),
    #[cfg(guest_arch = "x86_64")]
    CheckTimers(FailableRpc<(), ()>),
    #[cfg(guest_arch = "x86_64")]
    AdvanceLapic(FailableRpc<virt::x86::vp::LapicRestoreClock, ()>),
    #[cfg(guest_arch = "x86_64")]
    PrepareTimeAbiRestore(mesh::rpc::Rpc<(), ()>),
}

/// The LAPIC timers that a time ABI restore took out of the VPs' saved state.
#[cfg(guest_arch = "x86_64")]
pub(super) type RestoredTimers = Vec<(virt::VpIndex, virt::x86::vp::SavedOneShotTimer)>;

/// Returns `data`, a VP's saved state, prepared for a time ABI restore:
/// without its saved TSC, and with its LAPIC timer taken out, which this
/// returns with the saved TSC that dates its count.
#[cfg(guest_arch = "x86_64")]
pub(super) fn for_time_abi_restore(
    data: vmcore::save_restore::SavedStateBlob,
) -> Result<
    (
        vmcore::save_restore::SavedStateBlob,
        Option<virt::x86::vp::SavedOneShotTimer>,
    ),
    vmcore::save_restore::RestoreError,
> {
    let mut state: virt::vp::VpSavedState = data.parse()?;
    let timer = state.take_one_shot_timer();
    state.clear_tsc();
    Ok((vmcore::save_restore::SavedStateBlob::new(state), timer))
}

/// Returns the number of VPs to instantiate, validated against the topology's
/// VP count.
pub(super) fn active_vp_count(params: &PartitionUnitParams<'_>) -> Result<u32, Error> {
    let vp_capacity = params.processor_topology.vp_count();
    let active_vp_count = params.active_vp_count.unwrap_or(vp_capacity);
    if !(1..=vp_capacity).contains(&active_vp_count) {
        return Err(Error::InvalidActiveVpCount {
            active: active_vp_count,
            capacity: vp_capacity,
        });
    }
    Ok(active_vp_count)
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

    /// Checks the LAPIC timer of every stopped vCPU for the NVX time ABI
    /// (`E_LAPIC_PERIODIC`, `E_LAPIC_TSC_DEADLINE`).
    #[cfg(guest_arch = "x86_64")]
    pub async fn check_one_shot_timers(&mut self) -> anyhow::Result<()> {
        self.req_send
            .call_failable(
                |rpc| PartitionRequest::Snapshot(SnapshotRequest::CheckTimers(rpc)),
                (),
            )
            .await?;
        Ok(())
    }

    /// Advances the one-shot LAPIC timer of every stopped vCPU over the
    /// restore's `clock` and sets every vCPU's LAPIC state again, for the NVX
    /// time ABI. A timer that the restore took out of a vCPU's saved state is
    /// set again from that state.
    #[cfg(guest_arch = "x86_64")]
    pub async fn advance_lapic_timers(
        &mut self,
        clock: virt::x86::vp::LapicRestoreClock,
    ) -> anyhow::Result<()> {
        self.req_send
            .call_failable(
                |rpc| PartitionRequest::Snapshot(SnapshotRequest::AdvanceLapic(rpc)),
                clock,
            )
            .await?;
        Ok(())
    }

    /// Prepares every later restore for the NVX time ABI: drops the VPs'
    /// saved TSC values, which the synchronized TSC set supersedes, so no
    /// backend applies a per-VP TSC write before it, and takes each VP's
    /// LAPIC timer out of its restored APIC state, so no backend runs the
    /// timer before [`Self::advance_lapic_timers`] sets it again.
    #[cfg(guest_arch = "x86_64")]
    pub async fn prepare_time_abi_restore(&mut self) {
        self.req_send
            .call(
                |rpc| PartitionRequest::Snapshot(SnapshotRequest::PrepareTimeAbiRestore(rpc)),
                (),
            )
            .await
            .unwrap();
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
            SnapshotRequest::CheckTimers(rpc) => {
                rpc.handle_failable(async |()| self.vp_set.check_one_shot_timers().await)
                    .await
            }
            #[cfg(guest_arch = "x86_64")]
            SnapshotRequest::AdvanceLapic(rpc) => {
                rpc.handle_failable(async |clock| {
                    let timers = std::mem::take(&mut self.restored_timers);
                    self.vp_set.advance_lapic_timers(clock, timers).await
                })
                .await
            }
            #[cfg(guest_arch = "x86_64")]
            SnapshotRequest::PrepareTimeAbiRestore(rpc) => rpc.handle_sync(|()| {
                self.time_abi_restore = true;
            }),
        }
    }
}
