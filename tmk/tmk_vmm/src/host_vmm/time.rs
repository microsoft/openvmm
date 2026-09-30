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
use x86defs::apic::Lvt;
use x86defs::apic::TimerMode;

pub(super) struct Snapshot {
    vm: Vec<u8>,
    vp: Vec<u8>,
    memory: Vec<u8>,
    request: TimeCheckpoint,
    result: TimeCheckpointResult,
    reference: Option<u64>,
    deadline: Option<u64>,
    remaining: Option<u32>,
}

pub(super) fn check_initial_time(
    partition: &(impl Partition + PartitionAccessState),
    runner: &mut Runner<'_, impl Processor>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        runner.vp.access_state(Vtl::Vtl0).tsc()?.value == 0,
        "initial TSC is not zero"
    );
    if partition.caps().reference_time {
        anyhow::ensure!(
            partition.access_state(Vtl::Vtl0).reftime()?.value == 0,
            "initial reference time is not zero"
        );
    }
    Ok(())
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
    let reference = if partition.caps().reference_time {
        Some(partition.access_state(Vtl::Vtl0).reftime()?.value)
    } else {
        None
    };
    let deadline = if partition.caps().tsc_deadline {
        Some(runner.vp.access_state(Vtl::Vtl0).tsc_deadline()?.value)
    } else {
        None
    };
    let apic = runner.vp.access_state(Vtl::Vtl0).apic()?;
    let regs = apic.registers();
    let remaining =
        if regs.timer_icr != 0 && Lvt::from(regs.lvt_timer).timer_mode() == TimerMode::ONE_SHOT.0 {
            anyhow::ensure!(
                regs.timer_ccr > 0,
                "countdown expired before freeze rendezvous"
            );
            Some(regs.timer_ccr)
        } else {
            None
        };
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let elapsed_ns = start.elapsed().as_nanos().try_into()?;
    // These comparisons are deliberate, even for backends that cannot compare
    // every APIC field during generic state validation.
    anyhow::ensure!(
        runner.vp.access_state(Vtl::Vtl0).tsc()?.value == tsc,
        "frozen TSC advanced"
    );
    if let Some(reference) = reference {
        anyhow::ensure!(
            partition.access_state(Vtl::Vtl0).reftime()?.value == reference,
            "frozen reference time advanced"
        );
    }
    if let Some(deadline) = deadline {
        anyhow::ensure!(
            runner.vp.access_state(Vtl::Vtl0).tsc_deadline()?.value == deadline,
            "frozen deadline changed"
        );
    }
    if let Some(remaining) = remaining {
        anyhow::ensure!(
            runner
                .vp
                .access_state(Vtl::Vtl0)
                .apic()?
                .registers()
                .timer_ccr
                == remaining,
            "frozen countdown advanced"
        );
    }
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
        reference,
        deadline,
        remaining,
    };
    if matches!(request.action, TimeAction::Recreate) {
        return Ok(Some(snapshot));
    }

    partition
        .supports_reset()
        .context("partition reset unavailable")?
        .reset()?;
    runner.vp.reset()?;
    check_initial_time(partition, runner)?;
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
    anyhow::ensure!(
        runner.vp.access_state(Vtl::Vtl0).tsc()?.value == snapshot.result.tsc_before,
        "TSC did not survive serialized restore"
    );
    if let Some(reference) = snapshot.reference {
        anyhow::ensure!(
            partition.access_state(Vtl::Vtl0).reftime()?.value == reference,
            "reference time did not survive serialized restore"
        );
    }
    if let Some(deadline) = snapshot.deadline {
        anyhow::ensure!(
            runner.vp.access_state(Vtl::Vtl0).tsc_deadline()?.value == deadline,
            "deadline did not survive serialized restore"
        );
    }
    if let Some(remaining) = snapshot.remaining {
        let restored = runner
            .vp
            .access_state(Vtl::Vtl0)
            .apic()?
            .registers()
            .timer_ccr;
        anyhow::ensure!(
            restored.abs_diff(remaining) <= remaining / 100 + 1,
            "restore lost remaining countdown: {remaining} -> {restored}"
        );
    }
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
