// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-visible clock and native APIC timer continuity across full stops.

use super::apic::ApicMode;
use crate::prelude::*;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::Relaxed;
use tmk_protocol::TimeAction;
use x86defs::apic::ApicRegister;
use x86defs::apic::Dcr;
use x86defs::apic::Lvt;
use x86defs::apic::TimerMode;

const ACTIONS: [TimeAction; 3] = [
    TimeAction::Pause,
    TimeAction::SaveRestore,
    TimeAction::Recreate,
];
const TIMER_VECTOR: u8 = 0x40;
const OTHER_VECTOR: u8 = 0x50;
const TSC_DEADLINE_MSR: u32 = 0x6e0;

fn tsc() -> u64 {
    // SAFETY: LFENCE and RDTSC are supported in the x86-64 TMK environment.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        let value = core::arch::x86_64::_rdtsc();
        core::arch::x86_64::_mm_lfence();
        value
    }
}

fn calibrate() -> u64 {
    let result = tmk_core::time_checkpoint(TimeAction::Calibrate, 0);
    assert!(result.tsc_after > result.tsc_before);
    assert!(result.elapsed_ns >= 100_000_000);
    let rate = ((result.tsc_after - result.tsc_before) as u128 * 1_000_000_000
        / result.elapsed_ns as u128) as u64;
    assert!(rate > 0);
    rate
}

fn checkpoint(action: TimeAction, vector: u8, rate: u64) {
    let before = tsc();
    let result = tmk_core::time_checkpoint(action, vector);
    let after = tsc();
    log!(
        "checkpoint {:?}: guest {} ticks, host {} ns",
        action,
        after - before,
        result.elapsed_ns
    );
    assert!(result.elapsed_ns >= 1_000_000_000);
    assert!(before <= result.tsc_before);
    assert!(result.tsc_before <= result.tsc_after);
    assert!(result.tsc_after <= after);
    // Permit 250 ms of active host/guest scheduling, but not the second spent
    // frozen. Check RDTSC itself, not just the backend's cached state accessor.
    assert!(
        after - before < rate / 4,
        "guest TSC included the stopped interval"
    );
}

fn wait_until(deadline: u64) {
    while tsc() < deadline {
        core::hint::spin_loop();
    }
}

#[tmk_test(time_control)]
fn frozen_clock(_t: TestContext<'_>) {
    let rate = calibrate();
    for action in ACTIONS {
        checkpoint(action, 0, rate);
    }
}

#[tmk_test(time_control)]
fn frozen_countdown_xapic(t: TestContext<'_>) {
    countdown(t, ApicMode::XApic(x86defs::apic::APIC_BASE_ADDRESS));
}

#[tmk_test(time_control)]
fn frozen_countdown_x2apic(t: TestContext<'_>) {
    countdown(t, ApicMode::X2Apic);
}

fn countdown(t: TestContext<'_>, apic: ApicMode) {
    apic.init(t.scope);
    let rate = calibrate();
    apic.write(
        t.scope,
        ApicRegister::LVT_TIMER,
        Lvt::new().with_masked(true).into(),
    );
    apic.write(
        t.scope,
        ApicRegister::TIMER_DCR,
        Dcr::new().with_value_low(2).with_value_high(1).into(),
    );
    apic.write(t.scope, ApicRegister::TIMER_ICR, u32::MAX);
    let before = tsc();
    let count_before = apic.read(t.scope, ApicRegister::TIMER_CCR);
    tmk_core::time_checkpoint(TimeAction::Calibrate, 0);
    let count_after = apic.read(t.scope, ApicRegister::TIMER_CCR);
    let after = tsc();
    assert!(count_after > 0 && count_after < count_before);
    let initial =
        ((count_before - count_after) as u128 * rate as u128 / (after - before) as u128) as u32;
    assert!(initial > 100);
    apic.write(t.scope, ApicRegister::TIMER_ICR, 0);

    for action in ACTIONS {
        let fired = AtomicU64::new(0);
        let other = AtomicBool::new(false);
        let timer_isr = |_: &mut IsrContext<'_>| {
            fired.store(tsc(), Relaxed);
        };
        let other_isr = |_: &mut IsrContext<'_>| {
            other.store(true, Relaxed);
        };
        t.scope.subscope(|s| {
            s.disable_interrupts();
            s.set_isr(TIMER_VECTOR, &timer_isr);
            s.set_isr(OTHER_VECTOR, &other_isr);
            apic.write(
                s,
                ApicRegister::LVT_TIMER,
                Lvt::new().with_vector(TIMER_VECTOR).into(),
            );
            let started = tsc();
            apic.write(s, ApicRegister::TIMER_ICR, initial);
            wait_until(started + rate * 3 / 5);
            let remaining = apic.read(s, ApicRegister::TIMER_CCR);
            assert!(remaining > initial / 4 && remaining < initial / 2);
            checkpoint(action, OTHER_VECTOR, rate);
            let restored = apic.read(s, ApicRegister::TIMER_CCR);
            assert!(restored <= remaining && restored > remaining.saturating_sub(initial / 4));
            s.enable_interrupts();
            assert!(
                other.load(Relaxed),
                "unrelated interrupt was lost during timer restoration"
            );
            apic.write(s, ApicRegister::EOI, 0);
            let due = started + rate;
            while fired.load(Relaxed) == 0 && tsc() < due + rate / 4 {
                core::hint::spin_loop();
            }
            let delivered = fired.load(Relaxed);
            assert!(
                delivered >= due - rate / 10,
                "countdown fired early or was lost"
            );
            assert!(
                delivered <= due + rate / 4,
                "countdown restarted its full initial interval"
            );
            s.disable_interrupts();
            apic.write(s, ApicRegister::TIMER_ICR, 0);
            apic.write(s, ApicRegister::EOI, 0);
        });
    }
}

#[tmk_test(time_control, tsc_deadline)]
fn frozen_deadline_xapic(t: TestContext<'_>) {
    deadline(t, ApicMode::XApic(x86defs::apic::APIC_BASE_ADDRESS));
}

#[tmk_test(time_control, tsc_deadline)]
fn frozen_deadline_x2apic(t: TestContext<'_>) {
    deadline(t, ApicMode::X2Apic);
}

fn deadline(t: TestContext<'_>, apic: ApicMode) {
    apic.init(t.scope);
    let rate = calibrate();
    for action in ACTIONS {
        let fired = AtomicU64::new(0);
        let timer_isr = |_: &mut IsrContext<'_>| {
            fired.store(tsc(), Relaxed);
        };
        t.scope.subscope(|s| {
            s.disable_interrupts();
            s.set_isr(TIMER_VECTOR, &timer_isr);
            apic.write(
                s,
                ApicRegister::LVT_TIMER,
                Lvt::new()
                    .with_vector(TIMER_VECTOR)
                    .with_timer_mode(TimerMode::TSC_DEADLINE.0)
                    .into(),
            );
            let due = tsc() + rate * 2 / 5;
            s.write_msr(TSC_DEADLINE_MSR, due).unwrap();
            checkpoint(action, 0, rate);
            assert_eq!(s.read_msr(TSC_DEADLINE_MSR).unwrap(), due);
            s.enable_interrupts();
            while fired.load(Relaxed) == 0 && tsc() < due + rate / 4 {
                core::hint::spin_loop();
            }
            let delivered = fired.load(Relaxed);
            assert!(delivered >= due, "deadline fired early or was lost");
            assert!(delivered <= due + rate / 4, "deadline was delayed");
            s.disable_interrupts();
            apic.write(s, ApicRegister::EOI, 0);
            s.write_msr(TSC_DEADLINE_MSR, tsc() + rate * 2 / 5).unwrap();
            s.write_msr(TSC_DEADLINE_MSR, 0).unwrap();
            fired.store(0, Relaxed);
            checkpoint(action, 0, rate);
            assert_eq!(s.read_msr(TSC_DEADLINE_MSR).unwrap(), 0);
            s.enable_interrupts();
            wait_until(tsc() + rate / 2);
            assert_eq!(
                fired.load(Relaxed),
                0,
                "disarmed deadline fired after restore"
            );
        });
    }
}
