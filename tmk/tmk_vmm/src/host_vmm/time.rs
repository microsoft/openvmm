// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host half of the timekeeping TMK rendezvous. All operations run after the
//! single VP has left `run_vp`, not from inside an MMIO callback.

use crate::run::Runner;
use anyhow::Context as _;
use hvdef::Vtl;
use std::time::Duration;
use std::time::Instant;
use tmk_protocol::TimeAction;
use tmk_protocol::TimeCheckpoint;
use tmk_protocol::TimeCheckpointResult;
use virt::Partition;
use virt::PartitionAccessState;
use virt::Processor;
use virt::vm::AccessVmState as _;
use virt::vp::AccessVpState as _;

pub(super) struct Snapshot {
    vm: Vec<u8>,
    vp: Vec<u8>,
    memory: Vec<u8>,
    request: TimeCheckpoint,
    result: TimeCheckpointResult,
}

pub(super) fn checkpoint(
    partition: &(impl Partition + PartitionAccessState),
    runner: &mut Runner<'_, impl Processor>,
    request: TimeCheckpoint,
) -> anyhow::Result<Option<Snapshot>> {
    let control = partition
        .supports_time_control()
        .context("partition time control unavailable")?;
    if matches!(request.action, TimeAction::Calibrate) {
        let before = runner.vp.access_state(Vtl::Vtl0).tsc()?.value;
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        let after = runner.vp.access_state(Vtl::Vtl0).tsc()?.value;
        let result = TimeCheckpointResult {
            tsc_before: before,
            tsc_after: after,
            elapsed_ns: start.elapsed().as_nanos().try_into()?,
        };
        runner
            .guest_memory
            .write_plain(request.result_gpa, &result)?;
        return Ok(None);
    }

    control.freeze_time();
    let tsc = runner.vp.access_state(Vtl::Vtl0).tsc()?.value;
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let elapsed_ns = start.elapsed().as_nanos().try_into()?;
    let result = TimeCheckpointResult {
        tsc_before: tsc,
        tsc_after: 0,
        elapsed_ns,
    };
    if matches!(request.action, TimeAction::Pause) {
        resume(partition, runner, request, result)?;
        return Ok(None);
    }

    let vm = mesh::payload::encode(partition.access_state(Vtl::Vtl0).save_all()?);
    let vp = mesh::payload::encode(runner.vp.access_state(Vtl::Vtl0).save_all()?);
    let mut memory = vec![
        0;
        runner
            .guest_memory
            .inner_buf()
            .context("TMK memory is not contiguous")?
            .len()
    ];
    runner.guest_memory.read_at(0, &mut memory)?;
    let snapshot = Snapshot {
        vm,
        vp,
        memory,
        request,
        result,
    };
    if matches!(request.action, TimeAction::Recreate) {
        return Ok(Some(snapshot));
    }

    partition
        .supports_reset()
        .context("partition reset unavailable")?
        .reset()?;
    runner.vp.reset()?;
    restore(partition, runner, snapshot)?;
    Ok(None)
}

pub(super) fn restore(
    partition: &(impl Partition + PartitionAccessState),
    runner: &mut Runner<'_, impl Processor>,
    snapshot: Snapshot,
) -> anyhow::Result<()> {
    let vm = mesh::payload::decode::<virt::vm::VmSavedState>(&snapshot.vm)?;
    let vp = mesh::payload::decode::<virt::vp::VpSavedState>(&snapshot.vp)?;
    runner.guest_memory.write_at(0, &snapshot.memory)?;
    partition.access_state(Vtl::Vtl0).restore_all(&vm)?;
    runner.vp.access_state(Vtl::Vtl0).restore_all(&vp)?;
    resume(partition, runner, snapshot.request, snapshot.result)
}

fn resume(
    partition: &impl Partition,
    runner: &mut Runner<'_, impl Processor>,
    request: TimeCheckpoint,
    mut result: TimeCheckpointResult,
) -> anyhow::Result<()> {
    if request.interrupt_vector != 0 {
        partition.request_msi(
            Vtl::Vtl0,
            virt::irqcon::MsiRequest::new_x86(
                virt::irqcon::DeliveryMode::FIXED,
                0,
                false,
                request.interrupt_vector as u8,
                false,
            ),
        );
    }
    partition
        .supports_time_control()
        .context("partition time control unavailable")?
        .thaw_time();
    result.tsc_after = runner.vp.access_state(Vtl::Vtl0).tsc()?.value;
    runner
        .guest_memory
        .write_plain(request.result_gpa, &result)?;
    Ok(())
}
