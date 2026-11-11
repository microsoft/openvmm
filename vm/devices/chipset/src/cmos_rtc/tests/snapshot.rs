// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the snapshot fields of the RTC saved state and for advancing
//! the guest-visible time.

use super::get_cmos_data;
use super::new_test_rtc;
use super::set_cmos_data;
use crate::cmos_rtc::Rtc;
use crate::cmos_rtc::spec::CmosReg;
use chipset_device::poll_device::PollDevice;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
use local_clock::MockLocalClock;
use local_clock::MockLocalClockAccessor;
use local_clock::SystemTimeClock;
use pal_async::DefaultPool;
use pal_async::timer::Instant;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::SaveRestore;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;

/// Builds an RTC with a UTC clock of its own on the VM time of `keeper`, as a
/// restore builds it in a new process.
fn new_restored_rtc(
    pool: &mut DefaultPool,
    keeper: &VmTimeKeeper,
) -> (MockLocalClockAccessor, Rtc) {
    let driver = pool.driver();
    let source = pool.run_until(keeper.builder().build(&driver)).unwrap();
    let clock = MockLocalClock::new();
    let accessor = clock.accessor();
    let rtc = Rtc::new(
        Box::new(clock),
        LineInterrupt::detached(),
        &source,
        0x32,
        None,
        false,
    );
    (accessor, rtc)
}

/// Sets the stopped VM time to `millis`.
fn set_vmtime(pool: &mut DefaultPool, keeper: &mut VmTimeKeeper, millis: u64) {
    pool.run_until(
        keeper.restore(vmcore::vmtime::SavedState::from_vmtime(VmTime::from_100ns(
            millis * 10_000,
        ))),
    );
}

fn poll(rtc: &mut Rtc) {
    rtc.poll_device(&mut Context::from_waker(Waker::noop()));
}

/// Builds an RTC on the host's clock, as OpenVMM builds it.
fn new_host_clock_rtc(pool: &mut DefaultPool) -> (VmTimeKeeper, Rtc) {
    let driver = pool.driver();
    let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
    let source = pool.run_until(keeper.builder().build(&driver)).unwrap();
    let rtc = Rtc::new(
        Box::new(SystemTimeClock::new(LocalClockDelta::ZERO)),
        LineInterrupt::detached(),
        &source,
        0x32,
        None,
        false,
    );
    (keeper, rtc)
}

/// Returns the host's time in milliseconds from the Unix epoch.
fn host_millis() -> i64 {
    LocalClockTime::from(std::time::SystemTime::now()).as_millis_since_unix_epoch()
}

/// Returns the first and the last guest-visible time, in milliseconds from
/// the Unix epoch, that an RTC restores or advances to: the times that the
/// guest can write to the calendar registers, from 0000-01-01T00:00:00Z to
/// the last whole second that jiff represents in UTC, widened on both sides
/// by the range of the Linux host clock, `KTIME_SEC_MAX` seconds.
fn clock_time_range() -> (i64, i64) {
    const KTIME_SEC_MAX: i64 = i64::MAX / 1_000_000_000;
    let first: jiff::Timestamp = "0000-01-01T00:00:00Z".parse().unwrap();
    let last: jiff::Timestamp = "9999-12-30T22:00:00Z".parse().unwrap();
    (
        first.as_millisecond() - KTIME_SEC_MAX * 1000,
        last.as_millisecond() + KTIME_SEC_MAX * 1000,
    )
}

#[test]
fn restore_rejects_inconsistent_status_c_before_mutating_state() {
    use vmcore::save_restore::{ProtobufSaveRestore, SavedStateBlob};

    for status_c in [0x10, 0x80] {
        let (_pool, _keeper, _, mut rtc) = new_test_rtc();
        let before = SaveRestore::save(&mut rtc).unwrap();
        let mut invalid = SaveRestore::save(&mut rtc).unwrap();
        invalid.cmos[CmosReg::STATUS_C.0 as usize] = status_c;

        let error =
            ProtobufSaveRestore::restore(&mut rtc, SavedStateBlob::new(invalid)).unwrap_err();
        match error {
            vmcore::save_restore::RestoreError::InvalidSavedState(error) => {
                assert_eq!(
                    error.to_string(),
                    "inconsistent RTC status C interrupt flags"
                )
            }
            error => panic!("unexpected restore error: {error}"),
        }

        let after = SaveRestore::save(&mut rtc).unwrap();
        assert_eq!(after.addr, before.addr);
        assert_eq!(after.cmos, before.cmos);
        assert_eq!(after.clock_time_millis, before.clock_time_millis);
        assert_eq!(after.time_valid, before.time_valid);
    }
}

#[test]
fn restore_preserves_guest_epoch_and_advances_downtime() {
    let (mut pool, _vm_time_keeper, clock, mut rtc) = new_test_rtc();
    clock.tick(Duration::from_secs(10));
    let captured_time = clock.get_time();
    let saved = rtc.save().unwrap();

    clock.tick(Duration::from_secs(60));
    rtc.restore(saved).unwrap();
    assert_eq!(rtc.utc_time().unwrap(), captured_time);

    pool.run_until(rtc.advance_time(Duration::from_secs(3)))
        .unwrap();
    // The restored and advanced time reaches the clock as the RTC starts.
    rtc.start();
    assert_eq!(
        clock.get_time() - captured_time,
        Duration::from_secs(3).into()
    );
    // The RTC keeps its own guest-visible time, so the state unit that wraps
    // it must receive the advance.
    assert!(rtc.advances_time());
}

#[test]
fn restore_restores_the_calendar_validity() {
    for time_valid in [false, true] {
        let (_, _, _, mut rtc) = new_test_rtc();
        let mut saved = rtc.save().unwrap();
        saved.time_valid = time_valid;

        rtc.restore(saved).unwrap();
        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_D),
            if time_valid { 0x80 } else { 0 }
        );
    }
}

#[test]
fn restore_keeps_the_alarm_deadline() {
    let (mut pool, mut keeper, clock, mut rtc) = new_test_rtc();
    // Binary, 24-hour registers, at 00:00:59.300.
    set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x06);
    set_cmos_data(&mut rtc, CmosReg::SECOND, 59);
    clock.tick(Duration::from_millis(300));
    // An alarm at 00:01:00 that is armed at VM time 0 is due at VM time 1 s,
    // the next second of the calendar registers.
    set_cmos_data(&mut rtc, CmosReg::HOUR_ALARM, 0);
    set_cmos_data(&mut rtc, CmosReg::MINUTE_ALARM, 1);
    set_cmos_data(&mut rtc, CmosReg::SECOND_ALARM, 0);
    set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x26);
    // The snapshot is taken 800 ms later, when the clock reads 00:01:00.100.
    clock.tick(Duration::from_millis(800));
    set_vmtime(&mut pool, &mut keeper, 800);
    pool.run_until(rtc.stop());
    let saved = rtc.save().unwrap();

    let (_, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    set_vmtime(&mut pool, &mut keeper, 1001);
    restored.start();
    poll(&mut restored);
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_C), 0xA0);
}

#[test]
fn restore_keeps_the_phase_of_the_periodic_and_update_interrupts() {
    let (mut pool, mut keeper, _, mut rtc) = new_test_rtc();
    // A 2 Hz periodic interrupt and the update interrupt, started at VM time
    // 0, are due at VM times 0.5 s and 1 s.
    set_cmos_data(&mut rtc, CmosReg::STATUS_A, 0x2F);
    set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x56);
    set_vmtime(&mut pool, &mut keeper, 300);
    pool.run_until(rtc.stop());
    let saved = rtc.save().unwrap();

    let (_, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    set_vmtime(&mut pool, &mut keeper, 501);
    restored.start();
    poll(&mut restored);
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_C), 0xC0);

    set_vmtime(&mut pool, &mut keeper, 1001);
    poll(&mut restored);
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_C), 0xD0);
}

#[test]
fn restore_keeps_the_update_in_progress_phase() {
    let (mut pool, keeper, clock, mut rtc) = new_test_rtc();
    // The first read reports an update in progress, and the next one, 300 ms
    // later, does not.
    assert_eq!(get_cmos_data(&mut rtc, CmosReg::STATUS_A), 0xA6);
    clock.tick(Duration::from_millis(300));
    assert_eq!(get_cmos_data(&mut rtc, CmosReg::STATUS_A), 0x26);
    pool.run_until(rtc.stop());
    let saved = rtc.save().unwrap();

    let (restored_clock, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    restored.start();
    // The next update is in progress more than a second after the last one.
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_A), 0x26);
    restored_clock.tick(Duration::from_millis(700));
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_A), 0x26);
    restored_clock.tick(Duration::from_millis(1));
    assert_eq!(get_cmos_data(&mut restored, CmosReg::STATUS_A), 0xA6);
}

#[test]
fn restore_keeps_the_calendar_registers() {
    let (mut pool, keeper, clock, mut rtc) = new_test_rtc();
    // The calendar registers hold the time of the last read, 00:00:01, and
    // the alarm is armed.
    set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x26);
    assert_eq!(get_cmos_data(&mut rtc, CmosReg::SECOND), 1);
    clock.tick(Duration::from_secs(2));
    pool.run_until(rtc.stop());
    let saved = rtc.save().unwrap();
    let cmos = saved.cmos;

    let (_, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    assert_eq!(restored.state.cmos.0, cmos);
    // A read still returns the current time.
    restored.start();
    assert_eq!(get_cmos_data(&mut restored, CmosReg::SECOND), 3);
}

#[test]
fn restore_runs_the_clock_from_the_stop_with_vm_time() {
    let (mut pool, mut keeper, clock, mut rtc) = new_test_rtc();
    // The RTC stops at VM time 1 s, 200 ms before VM time stops at the
    // snapshot's cut, and the snapshot is saved 5 s later.
    set_vmtime(&mut pool, &mut keeper, 1000);
    pool.run_until(rtc.stop());
    set_vmtime(&mut pool, &mut keeper, 1200);
    clock.tick(Duration::from_millis(200));
    let cut = clock.get_time();
    clock.tick(Duration::from_secs(5));
    let saved = rtc.save().unwrap();
    assert_eq!(saved.clock_time_millis, cut.as_millis_since_unix_epoch());

    // The restore takes 7 s, then advances VM time and the RTC by a downtime
    // of 3 s, and VM time runs for 250 ms before the RTC starts.
    let (restored_clock, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    restored_clock.tick(Duration::from_secs(7));
    pool.run_until(keeper.advance(Duration::from_secs(3), Instant::now()));
    pool.run_until(restored.advance_time(Duration::from_secs(3)))
        .unwrap();
    set_vmtime(&mut pool, &mut keeper, 4450);
    restored.start();
    assert_eq!(
        restored_clock.get_time() - cut,
        Duration::from_millis(3250).into()
    );
}

#[test]
fn a_pause_leaves_the_clock_running() {
    let (mut pool, mut keeper, clock, mut rtc) = new_test_rtc();
    pool.run_until(rtc.stop());
    clock.tick(Duration::from_secs(5));
    set_vmtime(&mut pool, &mut keeper, 1000);
    let paused = clock.get_time();
    rtc.start();
    assert_eq!(clock.get_time(), paused);
}

#[test]
fn a_reset_runs_the_held_time_from_the_reset() {
    let (mut pool, mut keeper, clock, mut rtc) = new_test_rtc();
    set_vmtime(&mut pool, &mut keeper, 1000);
    pool.run_until(rtc.stop());
    let stopped = clock.get_time();
    // VM time resets to zero before the RTC does, then runs for 500 ms.
    set_vmtime(&mut pool, &mut keeper, 0);
    pool.run_until(rtc.reset());
    set_vmtime(&mut pool, &mut keeper, 500);
    assert_eq!(
        rtc.save().unwrap().clock_time_millis,
        stopped.as_millis_since_unix_epoch() + 500
    );
}

#[test]
fn restore_without_deadlines_arms_the_enabled_timers() {
    // A saved state without the deadlines, of an RTC whose 2 Hz periodic and
    // update interrupts are enabled, arms them a period after the restore.
    let (mut pool, mut keeper, _, mut rtc) = new_test_rtc();
    set_cmos_data(&mut rtc, CmosReg::STATUS_A, 0x2F);
    set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x56);
    let mut saved = rtc.save().unwrap();
    saved.periodic_deadline = None;
    saved.update_deadline = None;

    set_vmtime(&mut pool, &mut keeper, 300);
    let (_, mut restored) = new_restored_rtc(&mut pool, &keeper);
    restored.restore(saved).unwrap();
    assert_eq!(
        restored.vmtimer_periodic.deadline(),
        Some(VmTime::from_100ns(800 * 10_000))
    );
    assert_eq!(
        restored.vmtimer_update.deadline(),
        Some(VmTime::from_100ns(1300 * 10_000))
    );
}

#[test]
fn restore_rejects_a_deadline_beyond_its_period_before_mutating_state() {
    use vmcore::save_restore::RestoreError;

    // The alarm, a 2 Hz periodic interrupt, and the update interrupt, armed at
    // VM time 0, at most a day, 500 ms, and 1 s ahead of it.
    for (timer, limit_millis) in [("alarm", 86_400_000), ("periodic", 500), ("update", 1000)] {
        let (_pool, _keeper, _, mut rtc) = new_test_rtc();
        set_cmos_data(&mut rtc, CmosReg::STATUS_A, 0x2F);
        set_cmos_data(&mut rtc, CmosReg::STATUS_B, 0x76);
        let before = rtc.save().unwrap();
        let mut with_deadline = |millis: u64| {
            let mut saved = rtc.save().unwrap();
            let deadline = Some(VmTime::from_100ns(millis * 10_000));
            match timer {
                "alarm" => saved.alarm_deadline = deadline,
                "periodic" => saved.periodic_deadline = deadline,
                _ => saved.update_deadline = deadline,
            }
            saved
        };
        let invalid = with_deadline(limit_millis + 1);
        let at_limit = with_deadline(limit_millis);

        match rtc.restore(invalid).unwrap_err() {
            RestoreError::InvalidSavedState(error) => assert!(
                error
                    .to_string()
                    .starts_with(&format!("RTC {timer} deadline is ")),
                "{error}"
            ),
            error => panic!("unexpected restore error: {error}"),
        }
        let after = rtc.save().unwrap();
        assert_eq!(after.cmos, before.cmos);
        assert_eq!(after.alarm_deadline, before.alarm_deadline);
        assert_eq!(after.periodic_deadline, before.periodic_deadline);
        assert_eq!(after.update_deadline, before.update_deadline);

        // A deadline a whole period ahead is valid.
        rtc.restore(at_limit).unwrap();
    }
}

#[test]
fn restore_rejects_a_clock_time_out_of_range_before_mutating_state() {
    use vmcore::save_restore::RestoreError;

    let (first, last) = clock_time_range();
    for millis in [i64::MIN, first - 1, last + 1, i64::MAX] {
        let (mut pool, _keeper, clock, mut rtc) = new_test_rtc();
        pool.run_until(rtc.stop());
        let before = rtc.save().unwrap();
        let mut invalid = rtc.save().unwrap();
        invalid.clock_time_millis = millis;

        match rtc.restore(invalid).unwrap_err() {
            RestoreError::InvalidSavedState(error) => {
                assert!(error.to_string().starts_with("RTC clock time "), "{error}")
            }
            error => panic!("unexpected restore error: {error}"),
        }
        let after = rtc.save().unwrap();
        assert_eq!(after.clock_time_millis, before.clock_time_millis);
        assert_eq!(after.cmos, before.cmos);
        assert_eq!(after.time_valid, before.time_valid);
        // The RTC starts with the time that it held before the restore.
        rtc.start();
        assert_eq!(
            clock.get_time().as_millis_since_unix_epoch(),
            before.clock_time_millis
        );
    }

    // The ends of the range restore, and reach the clock as the RTC starts.
    for millis in [first, last] {
        let (mut pool, _keeper, clock, mut rtc) = new_test_rtc();
        pool.run_until(rtc.stop());
        let mut saved = rtc.save().unwrap();
        saved.clock_time_millis = millis;
        rtc.restore(saved).unwrap();
        rtc.start();
        assert_eq!(clock.get_time().as_millis_since_unix_epoch(), millis);
    }
}

#[test]
fn restore_bounds_the_clock_time_on_the_host_clock() {
    use vmcore::save_restore::RestoreError;

    // The host's clock keeps the guest-visible time as an offset from the
    // host's time, which a time near either end of `i64` overflows.
    let (first, last) = clock_time_range();
    let mut pool = DefaultPool::new();
    let (_keeper, mut rtc) = new_host_clock_rtc(&mut pool);
    for millis in [i64::MIN, i64::MAX] {
        pool.run_until(rtc.stop());
        let mut invalid = rtc.save().unwrap();
        invalid.clock_time_millis = millis;
        assert!(matches!(
            rtc.restore(invalid),
            Err(RestoreError::InvalidSavedState(_))
        ));
        rtc.start();
        let before = host_millis();
        let read = rtc.save().unwrap().clock_time_millis;
        assert!((before..=host_millis()).contains(&read), "{read}");
    }

    // The clock holds and runs from either end of the range.
    for millis in [first, last] {
        pool.run_until(rtc.stop());
        let mut saved = rtc.save().unwrap();
        saved.clock_time_millis = millis;
        let before = host_millis();
        rtc.restore(saved).unwrap();
        rtc.start();
        let read = rtc.save().unwrap().clock_time_millis;
        assert!(
            (millis..=millis + (host_millis() - before)).contains(&read),
            "{read}"
        );
    }
}

#[test]
fn advance_time_rejects_a_time_out_of_range_before_mutating_state() {
    let (_, last) = clock_time_range();
    for stopped in [false, true] {
        let (mut pool, _keeper, clock, mut rtc) = new_test_rtc();
        if stopped {
            pool.run_until(rtc.stop());
        }
        let now = clock.get_time().as_millis_since_unix_epoch();
        let before = rtc.save().unwrap();
        // A downtime past the end of the range fails and leaves the time as it
        // was, even when the advanced time fits in an `i64`.
        for downtime in [i64::MAX - now - 2, last - now + 1] {
            assert!(
                pool.run_until(rtc.advance_time(Duration::from_millis(downtime as u64)))
                    .is_err()
            );
            let after = rtc.save().unwrap();
            assert_eq!(after.clock_time_millis, now);
            assert_eq!(after.cmos, before.cmos);
        }
        if stopped {
            rtc.start();
            assert_eq!(clock.get_time().as_millis_since_unix_epoch(), now);
            pool.run_until(rtc.stop());
        }

        // A downtime to the end of the range advances the time to it, and no
        // further.
        pool.run_until(rtc.advance_time(Duration::from_millis((last - now) as u64)))
            .unwrap();
        assert!(
            pool.run_until(rtc.advance_time(Duration::from_millis(1)))
                .is_err()
        );
        rtc.start();
        assert_eq!(clock.get_time().as_millis_since_unix_epoch(), last);
    }
}

#[test]
fn advance_time_bounds_the_clock_time_on_the_host_clock() {
    // A downtime that leaves the host's clock a second short of the end of
    // `i64` overflows the clock's next read after that second.
    let mut pool = DefaultPool::new();
    let (_keeper, mut rtc) = new_host_clock_rtc(&mut pool);
    for stopped in [false, true] {
        if stopped {
            pool.run_until(rtc.stop());
        }
        let before = host_millis();
        let downtime = Duration::from_millis((i64::MAX - before - 1000) as u64);
        assert!(pool.run_until(rtc.advance_time(downtime)).is_err());
        rtc.start();
        let read = rtc.save().unwrap().clock_time_millis;
        assert!((before..=host_millis()).contains(&read), "{read}");
    }
}
