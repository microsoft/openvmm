// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests through the public virt interfaces.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use std::time::Duration;
use virt::Partition;
use virt::PartitionAccessState;
use virt::Processor;
use virt::vm;
use virt::vm::AccessVmState as _;
use virt::vp;
use virt::vp::AccessVpState as _;
use x86defs::apic::ApicBase;
use x86defs::apic::Dcr;
use x86defs::apic::Lvt;
use x86defs::apic::TimerMode;

const FROZEN_WAIT: Duration = Duration::from_millis(100);
const RUNNING_WAIT: Duration = Duration::from_millis(20);
const TSC: u64 = 123_456_789;
const REFERENCE_TIME: u64 = 30_000_000;
const DEADLINE: u64 = TSC + (1 << 48);
const INITIAL_COUNT: u32 = 1_000_000_000;
const REMAINING_COUNT: u32 = INITIAL_COUNT / 2;

backend_test!(initial_frozen, requires: [time_control]);
fn initial_frozen(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let initial = ClockState::read(partition, processor)?;
    initial.check_frozen(partition, processor)
}

backend_test!(reset_clocks, requires: [time_control, reset]);
fn reset_clocks(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    seed_clocks(partition, processor)?;
    reset(partition, processor)?;
    let expected = ClockState {
        tsc: 0,
        reference: partition.caps().reference_time.then_some(0),
    };
    expected.check(partition, processor)?;
    expected.check_frozen(partition, processor)
}

backend_test!(clock_restore, requires: [time_control, reset]);
fn clock_restore(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let expected = seed_clocks(partition, processor)?;
    let saved = SavedState::save(partition, processor)?;
    reset(partition, processor)?;
    saved.restore(partition, processor)?;
    expected.check(partition, processor)?;
    expected.check_frozen(partition, processor)
}

#[derive(Debug, PartialEq, Eq)]
struct ClockState {
    tsc: u64,
    reference: Option<u64>,
}

impl ClockState {
    fn read(
        partition: &(impl Partition + PartitionAccessState),
        processor: &mut impl Processor,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            tsc: processor.access_state(Vtl::Vtl0).tsc()?.value,
            reference: if partition.caps().reference_time {
                Some(partition.access_state(Vtl::Vtl0).reftime()?.value)
            } else {
                None
            },
        })
    }

    fn check(
        &self,
        partition: &(impl Partition + PartitionAccessState),
        processor: &mut impl Processor,
    ) -> anyhow::Result<()> {
        let actual = Self::read(partition, processor)?;
        anyhow::ensure!(
            actual == *self,
            "clock state mismatch: expected {self:?}, actual {actual:?}"
        );
        Ok(())
    }

    fn check_frozen(
        &self,
        partition: &(impl Partition + PartitionAccessState),
        processor: &mut impl Processor,
    ) -> anyhow::Result<()> {
        std::thread::sleep(FROZEN_WAIT);
        self.check(partition, processor)
            .context("clock state changed while frozen")
    }
}

fn seed_clocks(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<ClockState> {
    let expected = ClockState {
        tsc: TSC,
        reference: partition.caps().reference_time.then_some(REFERENCE_TIME),
    };
    if let Some(value) = expected.reference {
        let mut state = partition.access_state(Vtl::Vtl0);
        state.set_reftime(&vm::ReferenceTime { value })?;
        state.commit()?;
    }
    {
        let mut state = processor.access_state(Vtl::Vtl0);
        state.set_tsc(&vp::Tsc { value: TSC })?;
        state.commit()?;
    }
    expected.check(partition, processor)?;
    Ok(expected)
}

backend_test!(freeze_thaw, requires: [time_control]);
fn freeze_thaw(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let control = partition
        .supports_time_control()
        .context("partition time control unavailable")?;
    let before = seed_clocks(partition, processor)?;
    control.thaw_time();
    std::thread::sleep(RUNNING_WAIT);
    let running = ClockState::read(partition, processor)?;
    anyhow::ensure!(running.tsc > before.tsc, "TSC did not advance after thaw");
    if let (Some(before), Some(after)) = (before.reference, running.reference) {
        anyhow::ensure!(after > before, "reference time did not advance after thaw");
    }
    control.thaw_time();
    let still_running = ClockState::read(partition, processor)?;
    anyhow::ensure!(
        still_running.tsc >= running.tsc,
        "repeated thaw moved TSC backward"
    );
    if let (Some(before), Some(after)) = (running.reference, still_running.reference) {
        anyhow::ensure!(
            after >= before,
            "repeated thaw moved reference time backward"
        );
    }
    control.freeze_time();
    let frozen = ClockState::read(partition, processor)?;
    frozen.check_frozen(partition, processor)?;
    control.freeze_time();
    frozen.check(partition, processor)?;
    Ok(())
}

fn reset(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    partition
        .supports_reset()
        .context("selected backend does not support partition reset")?
        .reset()?;
    processor.reset()?;
    Ok(())
}

struct SavedState {
    vm: Vec<u8>,
    vp: Vec<u8>,
}

impl SavedState {
    fn save(
        partition: &(impl Partition + PartitionAccessState),
        processor: &mut impl Processor,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            vm: mesh::payload::encode(partition.access_state(Vtl::Vtl0).save_all()?),
            vp: mesh::payload::encode(processor.access_state(Vtl::Vtl0).save_all()?),
        })
    }

    fn restore(
        &self,
        partition: &(impl Partition + PartitionAccessState),
        processor: &mut impl Processor,
    ) -> anyhow::Result<()> {
        let vm = mesh::payload::decode::<vm::VmSavedState>(&self.vm)?;
        let vp = mesh::payload::decode::<vp::VpSavedState>(&self.vp)?;
        partition.access_state(Vtl::Vtl0).restore_all(&vm)?;
        processor.access_state(Vtl::Vtl0).restore_all(&vp)?;
        Ok(())
    }
}

fn set_timer_mode(processor: &mut impl Processor, mode: TimerMode) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let mut apic = state.apic()?;
    apic.apic_base = ApicBase::from(apic.apic_base).with_enable(true).into();
    let mut regs = *apic.registers();
    regs.svr = 0x1ff;
    regs.lvt_timer = Lvt::new().with_vector(0x40).with_timer_mode(mode.0).into();
    regs.timer_dcr = Dcr::new().with_value_low(2).with_value_high(1).into();
    if mode == TimerMode::ONE_SHOT {
        regs.timer_icr = INITIAL_COUNT;
        regs.timer_ccr = REMAINING_COUNT;
    } else {
        regs.timer_icr = 0;
        regs.timer_ccr = 0;
    }
    apic.registers = *regs.as_array();
    state.set_apic(&apic)?;
    state.commit()?;
    Ok(())
}

fn countdown(processor: &mut impl Processor) -> anyhow::Result<(u32, u32)> {
    let apic = processor.access_state(Vtl::Vtl0).apic()?;
    let regs = apic.registers();
    Ok((regs.timer_icr, regs.timer_ccr))
}

backend_test!(countdown_restore, requires: [time_control, reset]);
fn countdown_restore(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    seed_clocks(partition, processor)?;
    set_timer_mode(processor, TimerMode::ONE_SHOT)?;
    let (initial, remaining) = countdown(processor)?;
    anyhow::ensure!(initial == INITIAL_COUNT, "initial countdown was not set");
    anyhow::ensure!(
        remaining.abs_diff(REMAINING_COUNT) <= REMAINING_COUNT / 100 + 1,
        "remaining countdown was not set: {remaining}"
    );
    let control = partition
        .supports_time_control()
        .context("partition time control unavailable")?;
    control.thaw_time();
    std::thread::sleep(RUNNING_WAIT);
    control.freeze_time();
    let expected = countdown(processor)?;
    anyhow::ensure!(
        expected.0 == initial && expected.1 > 0 && expected.1 < remaining,
        "countdown did not run before freezing: {expected:?}"
    );
    std::thread::sleep(FROZEN_WAIT);
    anyhow::ensure!(
        countdown(processor)? == expected,
        "frozen countdown advanced"
    );
    let clocks = ClockState::read(partition, processor)?;
    let saved = SavedState::save(partition, processor)?;
    reset(partition, processor)?;
    saved.restore(partition, processor)?;
    let restored = countdown(processor)?;
    // Hardware timer representations may round the remaining count on restore.
    anyhow::ensure!(
        restored.0 == expected.0 && restored.1.abs_diff(expected.1) <= expected.1 / 100 + 1,
        "countdown did not survive restore: {expected:?} -> {restored:?}"
    );
    clocks.check_frozen(partition, processor)?;
    anyhow::ensure!(
        countdown(processor)? == restored,
        "restored countdown advanced while frozen"
    );
    Ok(())
}

fn set_deadline(processor: &mut impl Processor, value: u64) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    state.set_tsc_deadline(&vp::TscDeadline { value })?;
    state.commit()?;
    Ok(())
}

fn check_deadline(processor: &mut impl Processor, expected: u64) -> anyhow::Result<()> {
    let actual = processor.access_state(Vtl::Vtl0).tsc_deadline()?.value;
    anyhow::ensure!(
        actual == expected,
        "deadline mismatch: {expected:#x} -> {actual:#x}"
    );
    Ok(())
}

backend_test!(deadline_restore, requires: [time_control, reset, deadline]);
fn deadline_restore(
    partition: &(impl Partition + PartitionAccessState),
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        partition.caps().tsc_deadline,
        "selected backend does not expose TSC-deadline state"
    );
    seed_clocks(partition, processor)?;
    set_timer_mode(processor, TimerMode::TSC_DEADLINE)?;
    set_deadline(processor, DEADLINE)?;
    check_deadline(processor, DEADLINE)?;
    let control = partition
        .supports_time_control()
        .context("partition time control unavailable")?;
    control.thaw_time();
    std::thread::sleep(RUNNING_WAIT);
    control.freeze_time();
    check_deadline(processor, DEADLINE)?;
    let clocks = ClockState::read(partition, processor)?;
    let saved = SavedState::save(partition, processor)?;
    reset(partition, processor)?;
    saved.restore(partition, processor)?;
    check_deadline(processor, DEADLINE)?;
    clocks.check_frozen(partition, processor)?;
    check_deadline(processor, DEADLINE)?;

    set_deadline(processor, 0)?;
    let disarmed = SavedState::save(partition, processor)?;
    set_deadline(processor, DEADLINE)?;
    disarmed.restore(partition, processor)?;
    check_deadline(processor, 0)?;
    clocks.check_frozen(partition, processor)?;
    check_deadline(processor, 0)?;
    Ok(())
}
