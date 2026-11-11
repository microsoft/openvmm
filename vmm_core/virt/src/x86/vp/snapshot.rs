// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VP state for snapshot restore: LAPIC timer checks and advancement across
//! snapshot downtime under the NVX time ABI.

use super::Apic;
use super::ApicRegisters;
use crate::time_abi::TimeAbiCode;
use crate::time_abi::TimeAbiError;
use crate::time_abi::rate;

/// A one-shot LAPIC timer that a time ABI restore takes out of a VP's saved
/// APIC state with [`Apic::take_one_shot_timer`], so that no backend runs it
/// before the synchronized TSC set, and sets again after the set with
/// [`Apic::restore_one_shot_timer`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SavedOneShotTimer {
    /// The saved initial count.
    pub initial_count: u32,
    /// The saved current count.
    pub current_count: u32,
    /// The VP's saved TSC, if its saved state holds one. A save reads it
    /// after the APIC state, so it dates the saved current count.
    pub saved_tsc: Option<u64>,
}

/// The clocks of a time ABI restore that restored one-shot LAPIC timers
/// follow.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct LapicRestoreClock {
    /// The guest TSC at the capture anchor, `T_c`.
    pub capture_tsc: u64,
    /// The declared TSC rate `F_s`, in Hz, the best known rate of the TSC
    /// that the capture read.
    pub declared_tsc_hz: u64,
    /// The target of the synchronized TSC set, from which every VP's TSC
    /// runs after the set.
    pub restore_tsc: u64,
    /// The destination's native TSC rate `F_d`, in Hz, at which the TSC runs
    /// from the target.
    pub native_tsc_hz: u64,
    /// The downtime `D`, in nanoseconds.
    pub downtime_ns: u64,
    /// The LAPIC timer rate `L`, in Hz.
    pub apic_hz: u64,
}

/// Returns the TSC cycles from `earlier` to `later`, modulo 2^64, or 0 if
/// `later` is behind `earlier`, as a VP's TSC can be by the skew between
/// VPs.
fn tsc_cycles_after(later: u64, earlier: u64) -> u64 {
    let cycles = later.wrapping_sub(earlier);
    if cycles > i64::MAX as u64 { 0 } else { cycles }
}

/// Returns the divide value of the timer's divide configuration.
fn timer_divide(registers: &ApicRegisters) -> u32 {
    match registers.timer_dcr & 0xb {
        0xb => 1,
        0 => 2,
        1 => 4,
        2 => 8,
        3 => 16,
        8 => 32,
        9 => 64,
        0xa => 128,
        _ => unreachable!(),
    }
}

impl Apic {
    fn queue_timer_interrupt(&mut self, registers: &mut ApicRegisters) {
        if registers.lvt_timer & (1 << 16) != 0 {
            return;
        }
        let vector = (registers.lvt_timer & 0xff) as usize;
        if vector < 16 {
            return;
        }
        let bank = vector / 32;
        let mask = 1 << (vector % 32);
        registers.irr[bank] |= mask;
        registers.tmr[bank] &= !mask;
        self.auto_eoi[bank] &= !mask;
    }

    /// Counts a counting one-shot timer whose count is `count` down by
    /// `ticks`, by the time ABI's tick rule: if the ticks reach the count, the
    /// count becomes 0 and the timer interrupt is queued unless the LVT is
    /// masked. `count` must fit the current count register.
    fn count_down_one_shot_timer(&mut self, count: u64, ticks: u64) {
        let mut registers = ApicRegisters::from_array(self.registers);
        if ticks >= count {
            registers.timer_ccr = 0;
            self.queue_timer_interrupt(&mut registers);
        } else {
            registers.timer_ccr = u32::try_from(count - ticks).expect("the count fits 32 bits");
        }
        self.registers = *registers.as_array();
    }

    /// Checks the LAPIC timer against the NVX time ABI: an armed periodic
    /// timer fails with `E_LAPIC_PERIODIC`, and TSC-deadline (or reserved)
    /// timer mode with `E_LAPIC_TSC_DEADLINE`.
    pub fn check_one_shot_timer(&self) -> Result<(), TimeAbiError> {
        let registers = ApicRegisters::from_array(self.registers);
        match (registers.lvt_timer >> 17) & 0x3 {
            1 if registers.timer_icr != 0 => Err(TimeAbiError::new(
                TimeAbiCode::LapicPeriodic,
                format!(
                    "the LAPIC timer is periodic with initial count {:#x}",
                    registers.timer_icr
                ),
            )),
            mode @ (2 | 3) => Err(TimeAbiError::new(
                TimeAbiCode::LapicTscDeadline,
                format!("the LAPIC timer is in mode {mode}, not one-shot"),
            )),
            _ => Ok(()),
        }
    }

    /// Advances a counting-mode one-shot LAPIC timer over a downtime of
    /// `downtime_ns` at `apic_hz`, by the time ABI's tick rule: if the ticks
    /// reach the current count, the count becomes 0 and the timer interrupt
    /// is queued unless the LVT is masked; otherwise the count decreases by
    /// the ticks. Fails like [`Self::check_one_shot_timer`].
    pub fn advance_one_shot_timer(
        &mut self,
        downtime_ns: u64,
        apic_hz: u64,
    ) -> Result<(), TimeAbiError> {
        self.check_one_shot_timer()?;
        let registers = ApicRegisters::from_array(self.registers);
        if registers.timer_ccr == 0 {
            return Ok(());
        }
        let ticks = rate::lapic_ticks(downtime_ns, apic_hz, timer_divide(&registers))
            .expect("the divide configuration is a power of two up to 128");
        self.count_down_one_shot_timer(registers.timer_ccr.into(), ticks);
        Ok(())
    }

    /// Takes the LAPIC timer out of this saved APIC state, whose VP saved
    /// `saved_tsc` as its TSC, for a time ABI restore: sets the initial and
    /// current counts to 0, so that a backend that sets this state runs no
    /// timer, and returns them for [`Self::restore_one_shot_timer`].
    pub fn take_one_shot_timer(&mut self, saved_tsc: Option<u64>) -> SavedOneShotTimer {
        let mut registers = ApicRegisters::from_array(self.registers);
        let timer = SavedOneShotTimer {
            initial_count: registers.timer_icr,
            current_count: registers.timer_ccr,
            saved_tsc,
        };
        registers.timer_icr = 0;
        registers.timer_ccr = 0;
        self.registers = *registers.as_array();
        timer
    }

    /// Sets `timer`, which [`Self::take_one_shot_timer`] took from the saved
    /// state of this APIC, again once the guest TSC runs from the target of
    /// the synchronized TSC set, with the VP's TSC at `now_tsc`, and fails
    /// like [`Self::check_one_shot_timer`].
    ///
    /// A counting timer gets the count that it had at the capture anchor: its
    /// saved count plus the ticks of the saved TSC past `T_c`, at the declared
    /// rate and at most its initial count, since the save read the count
    /// after the anchor and the TSC after the count. That count advances by
    /// the downtime as [`Self::advance_one_shot_timer`] does, and by the ticks
    /// of the TSC past the target, at the native rate, so that the timer runs
    /// with the guest TSC. A timer with no initial or current count keeps its
    /// saved counts.
    pub fn restore_one_shot_timer(
        &mut self,
        timer: SavedOneShotTimer,
        clock: &LapicRestoreClock,
        now_tsc: u64,
    ) -> Result<(), TimeAbiError> {
        let mut registers = ApicRegisters::from_array(self.registers);
        registers.timer_icr = timer.initial_count;
        registers.timer_ccr = timer.current_count;
        self.registers = *registers.as_array();
        self.check_one_shot_timer()?;
        if timer.initial_count == 0 || timer.current_count == 0 {
            return Ok(());
        }
        let divide = timer_divide(&registers);
        // Only an overflow, from a TSC interval of decades, returns `None`.
        let ticks_after = |later: u64, earlier: u64, tsc_hz: u64| {
            rate::lapic_ticks_in_tsc(
                tsc_cycles_after(later, earlier),
                tsc_hz,
                clock.apic_hz,
                divide,
            )
            .unwrap_or(u64::MAX)
        };
        let past_capture = timer.saved_tsc.map_or(0, |tsc| {
            ticks_after(tsc, clock.capture_tsc, clock.declared_tsc_hz)
        });
        let count = u64::from(timer.current_count)
            .saturating_add(past_capture)
            .min(timer.initial_count.into());
        let ticks = rate::lapic_ticks(clock.downtime_ns, clock.apic_hz, divide)
            .expect("the divide configuration is a power of two up to 128")
            .saturating_add(ticks_after(now_tsc, clock.restore_tsc, clock.native_tsc_hz));
        self.count_down_one_shot_timer(count, ticks);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    const TIMER_VECTOR: u32 = 0x40;
    const TIMER_MASKED: u32 = 1 << 16;
    const TIMER_PERIODIC: u32 = 1 << 17;
    const TIMER_TSC_DEADLINE: u32 = 2 << 17;

    fn apic(lvt_timer: u32, initial_count: u32, current_count: u32) -> Apic {
        let registers = ApicRegisters {
            lvt_timer,
            timer_icr: initial_count,
            timer_ccr: current_count,
            timer_dcr: 0xb,
            ..FromZeros::new_zeroed()
        };
        Apic {
            apic_base: 0,
            registers: *registers.as_array(),
            auto_eoi: [0; 8],
        }
    }

    fn timer_pending(apic: &Apic) -> bool {
        let registers = apic.registers();
        registers.irr[TIMER_VECTOR as usize / 32] & (1 << (TIMER_VECTOR as usize % 32)) != 0
    }

    fn with_divide(apic: Apic, dcr: u32) -> Apic {
        let mut registers = ApicRegisters::from_array(apic.registers);
        registers.timer_dcr = dcr;
        Apic {
            registers: *registers.as_array(),
            ..apic
        }
    }

    #[test]
    fn time_abi_one_shot_timer_counts_down_by_the_tick_rule() {
        // D = 1 ms at L = 200 MHz is 200,000 ticks before division.
        for (dcr, divide) in [
            (0xb, 1),
            (0, 2),
            (1, 4),
            (2, 8),
            (3, 16),
            (8, 32),
            (9, 64),
            (0xa, 128),
        ] {
            let mut timer = with_divide(apic(TIMER_VECTOR, 1_000_000, 1_000_000), dcr);
            timer
                .advance_one_shot_timer(1_000_000, 200_000_000)
                .unwrap();
            assert_eq!(timer.registers().timer_ccr, 1_000_000 - 200_000 / divide);
            assert!(!timer_pending(&timer));
        }

        // floor(999 ns * 1 GHz / 1e9) = 999 ticks.
        let mut timer = apic(TIMER_VECTOR, 1_000, 1_000);
        timer.advance_one_shot_timer(999, 1_000_000_000).unwrap();
        assert_eq!(timer.registers().timer_ccr, 1);
        assert!(!timer_pending(&timer));
    }

    #[test]
    fn time_abi_one_shot_timer_expires_once() {
        let mut timer = apic(TIMER_VECTOR, 100, 50);
        timer.advance_one_shot_timer(50, 1_000_000_000).unwrap();
        assert_eq!(timer.registers().timer_ccr, 0);
        assert!(timer_pending(&timer));
        // An expired timer stays expired; nothing else is queued.
        timer
            .advance_one_shot_timer(1_000_000_000, 1_000_000_000)
            .unwrap();
        assert_eq!(timer.registers().timer_ccr, 0);

        let mut masked = apic(TIMER_VECTOR | TIMER_MASKED, 100, 50);
        masked
            .advance_one_shot_timer(1_000_000_000, 200_000_000)
            .unwrap();
        assert_eq!(masked.registers().timer_ccr, 0);
        assert!(!timer_pending(&masked));

        let mut idle = apic(TIMER_VECTOR, 0, 0);
        idle.advance_one_shot_timer(1_000_000_000, 200_000_000)
            .unwrap();
        assert!(!timer_pending(&idle));
    }

    #[test]
    fn time_abi_rejects_periodic_and_deadline_timers() {
        let periodic = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);
        assert_eq!(
            periodic.check_one_shot_timer().unwrap_err().code,
            TimeAbiCode::LapicPeriodic
        );
        let mut advanced = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);
        assert_eq!(
            advanced
                .advance_one_shot_timer(1_000, 200_000_000)
                .unwrap_err()
                .code,
            TimeAbiCode::LapicPeriodic
        );
        // A periodic LVT with no initial count is not armed.
        apic(TIMER_VECTOR | TIMER_PERIODIC, 0, 0)
            .check_one_shot_timer()
            .unwrap();

        for lvt in [TIMER_TSC_DEADLINE, 3 << 17] {
            assert_eq!(
                apic(TIMER_VECTOR | lvt, 0, 0)
                    .check_one_shot_timer()
                    .unwrap_err()
                    .code,
                TimeAbiCode::LapicTscDeadline
            );
        }
    }

    /// A 2 GHz TSC and a 1 GHz LAPIC timer: one TSC cycle is half a tick.
    fn clock(downtime_ns: u64) -> LapicRestoreClock {
        LapicRestoreClock {
            capture_tsc: 1_000_000,
            declared_tsc_hz: 2_000_000_000,
            restore_tsc: 9_000_000,
            native_tsc_hz: 2_000_000_000,
            downtime_ns,
            apic_hz: 1_000_000_000,
        }
    }

    /// Takes the timer out of `saved`, saved with `saved_tsc` as its TSC, and
    /// restores it with `clock` at `now_tsc`.
    fn restore(
        saved: &Apic,
        saved_tsc: Option<u64>,
        clock: &LapicRestoreClock,
        now_tsc: u64,
    ) -> Result<Apic, TimeAbiError> {
        let mut restored = saved.clone();
        let timer = restored.take_one_shot_timer(saved_tsc);
        restored.restore_one_shot_timer(timer, clock, now_tsc)?;
        Ok(restored)
    }

    #[test]
    fn time_abi_restore_takes_the_timer_out_of_the_restored_state() {
        let mut saved = apic(TIMER_VECTOR, 1_000, 400);
        let timer = saved.take_one_shot_timer(Some(7));
        assert_eq!(
            timer,
            SavedOneShotTimer {
                initial_count: 1_000,
                current_count: 400,
                saved_tsc: Some(7),
            }
        );
        let registers = saved.registers();
        assert_eq!((registers.timer_icr, registers.timer_ccr), (0, 0));
        assert_eq!(registers.lvt_timer, TIMER_VECTOR);
        assert_eq!(registers.timer_dcr, 0xb);
    }

    #[test]
    fn time_abi_restore_counts_the_timer_from_the_anchors() {
        let clock = clock(1_000);
        let saved = apic(TIMER_VECTOR, 100_000, 50_000);

        // Saved at the capture anchor and set at the restore anchor, the
        // timer advances by the downtime alone, as the tick rule says.
        let mut advanced = saved.clone();
        advanced
            .advance_one_shot_timer(1_000, 1_000_000_000)
            .unwrap();
        let restored = restore(&saved, Some(clock.capture_tsc), &clock, clock.restore_tsc).unwrap();
        assert_eq!(restored, advanced);
        assert_eq!(restored.registers().timer_ccr, 49_000);
        assert_eq!(restored.registers().timer_icr, 100_000);

        // Saved 4,000 cycles (2,000 ticks) after the capture anchor and set
        // 6,000 cycles (3,000 ticks) after the restore anchor: the count is
        // the one at the capture anchor, less the downtime and the ticks that
        // already ran from the restore anchor.
        let restored = restore(
            &saved,
            Some(clock.capture_tsc + 4_000),
            &clock,
            clock.restore_tsc + 6_000,
        )
        .unwrap();
        assert_eq!(
            restored.registers().timer_ccr,
            50_000 + 2_000 - 1_000 - 3_000
        );
        assert!(!timer_pending(&restored));

        // Without a saved TSC, or with one before the anchor, the saved count
        // is the count at the anchor.
        for saved_tsc in [None, Some(clock.capture_tsc - 1)] {
            let restored = restore(&saved, saved_tsc, &clock, clock.restore_tsc).unwrap();
            assert_eq!(restored.registers().timer_ccr, 49_000);
        }

        // The count at the capture anchor is at most the initial count.
        let restored = restore(
            &saved,
            Some(clock.capture_tsc + 1_000_000),
            &clock,
            clock.restore_tsc,
        )
        .unwrap();
        assert_eq!(restored.registers().timer_ccr, 100_000 - 1_000);

        // The divide configuration divides the ticks of both TSC intervals.
        let restored = restore(
            &with_divide(saved.clone(), 0),
            Some(clock.capture_tsc + 4_000),
            &clock,
            clock.restore_tsc + 6_000,
        )
        .unwrap();
        assert_eq!(restored.registers().timer_ccr, 50_000 + 1_000 - 500 - 1_500);

        // The capture's TSC counts at the declared rate, and the restored TSC
        // at the destination's native rate: here 4 GHz, a cycle a quarter
        // tick.
        let faster = LapicRestoreClock {
            native_tsc_hz: 4_000_000_000,
            ..clock
        };
        let restored = restore(
            &saved,
            Some(clock.capture_tsc + 4_000),
            &faster,
            clock.restore_tsc + 6_000,
        )
        .unwrap();
        assert_eq!(
            restored.registers().timer_ccr,
            50_000 + 2_000 - 1_000 - 1_500
        );

        // A restored TSC behind the anchor, as a VP's can be by the skew
        // between VPs, counts nothing, and one that wrapped past 2^64 counts
        // the cycles since the anchor.
        let restored = restore(
            &saved,
            Some(clock.capture_tsc),
            &clock,
            clock.restore_tsc - 1,
        )
        .unwrap();
        assert_eq!(restored.registers().timer_ccr, 49_000);
        let wrapping = LapicRestoreClock {
            restore_tsc: u64::MAX - 999,
            ..clock
        };
        let restored = restore(&saved, Some(clock.capture_tsc), &wrapping, 5_000).unwrap();
        assert_eq!(restored.registers().timer_ccr, 49_000 - 3_000);
    }

    #[test]
    fn time_abi_restore_expires_the_timer_once() {
        let clock = clock(1_000);
        // The downtime expires it.
        let saved = apic(TIMER_VECTOR, 100_000, 1_000);
        let restored = restore(&saved, Some(clock.capture_tsc), &clock, clock.restore_tsc).unwrap();
        assert_eq!(restored.registers().timer_ccr, 0);
        assert!(timer_pending(&restored));

        // So does the time from the restore anchor until it is set.
        let saved = apic(TIMER_VECTOR, 100_000, 2_000);
        let restored = restore(
            &saved,
            Some(clock.capture_tsc),
            &clock,
            clock.restore_tsc + 2_000,
        )
        .unwrap();
        assert_eq!(restored.registers().timer_ccr, 0);
        assert!(timer_pending(&restored));

        // A masked timer expires without an interrupt.
        let saved = apic(TIMER_VECTOR | TIMER_MASKED, 100_000, 1_000);
        let restored = restore(&saved, Some(clock.capture_tsc), &clock, clock.restore_tsc).unwrap();
        assert_eq!(restored.registers().timer_ccr, 0);
        assert!(!timer_pending(&restored));

        // A timer that expired before the capture keeps its counts and queues
        // nothing: whether its interrupt was taken is the backend's to say. An
        // idle timer stays idle.
        for (initial, current) in [(100_000, 0), (0, 0)] {
            let saved = apic(TIMER_VECTOR, initial, current);
            let restored = restore(
                &saved,
                Some(clock.capture_tsc + 1_000),
                &clock,
                clock.restore_tsc + 1_000_000,
            )
            .unwrap();
            assert_eq!(restored, saved);
        }
    }

    #[test]
    fn time_abi_restore_rejects_periodic_and_deadline_timers() {
        let clock = clock(1_000);
        // The checks see the saved counts, not the taken ones.
        let periodic = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);
        assert_eq!(
            restore(&periodic, None, &clock, clock.restore_tsc)
                .unwrap_err()
                .code,
            TimeAbiCode::LapicPeriodic
        );
        let deadline = apic(TIMER_VECTOR | TIMER_TSC_DEADLINE, 0, 0);
        assert_eq!(
            restore(&deadline, None, &clock, clock.restore_tsc)
                .unwrap_err()
                .code,
            TimeAbiCode::LapicTscDeadline
        );
    }
}
