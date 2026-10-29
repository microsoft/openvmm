// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The host's side of a time sample under time ABI 2: host UTC paired with
//! the guest TSC, and the discipline of host UTC, which the VMM publishes so
//! that the guest applies the host's clock correction instead of estimating
//! it.

use super::TimeAbiCode;
use super::TimeAbiError;
use super::TimeAbiTestHooks;
use super::host::HostClockBasis;
use super::host::HostClockDiscipline;
use super::host::HostClockKind;
use super::host::HostClockStatus;
use super::host::HostTimeSample;
use super::host::SCALED_PPM;
use super::tsc::GuestTscOffset;
use super::tsc::cycles_to_ns_ceil;

/// One, in parts per million scaled by 2^16.
const ONE: i128 = SCALED_PPM as i128 * 1_000_000;

/// A change of host UTC or of the host's suspended time at least this large,
/// in nanoseconds, beyond what the host's correction explains, is a
/// disruption.
pub const DISRUPTION_THRESHOLD_NS: u64 = 1_000_000;

/// How far the correction of a host clock that does not follow host UTC's
/// rate may change between two samples, in parts per million: Linux's limit
/// of a frequency correction.
const CORRECTION_CHANGE_PPM: i128 = 500;

/// The rounding of a published correction and of the host's conversion of
/// the TSC to time, in parts per million scaled by 2^16: one part per
/// billion.
pub const CORRECTION_ROUNDING: u64 = 66;

/// What a backend reports as the native TSC rate `F_d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRateBasis {
    /// The host kernel's calibration of the TSC, to the kHz, as KVM reports
    /// it.
    KernelKhz,
    /// The hypervisor's TSC rate, to the Hz, as MSHV and WHP report it.
    Hypervisor,
}

/// The rates to which a time sample's correction refers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRates {
    /// The declared TSC rate `F`, in Hz.
    pub declared_hz: u64,
    /// The native TSC rate `F_d`, in Hz.
    pub native_hz: u64,
    /// What `native_hz` measures.
    pub native_basis: NativeRateBasis,
}

/// The host's readings for one time sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostReadings {
    /// The host TSC at the instant of `time`.
    pub host_tsc: u64,
    /// Host UTC and host monotonic time at that instant.
    pub time: HostTimeSample,
    /// The most the instants of `host_tsc` and `time` can differ, in
    /// nanoseconds.
    pub pairing_ns: u64,
    /// The discipline of host UTC.
    pub discipline: HostClockDiscipline,
    /// The host's suspended time, as
    /// [`host_suspended_ns`](super::host::host_suspended_ns) reports it.
    pub suspended_ns: u64,
}

/// The host's fields of a time sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedSample {
    /// Host UTC, in nanoseconds since the Unix epoch, after the test hooks.
    pub utc_ns: u64,
    /// The guest TSC at the instant of `utc_ns`.
    pub tsc: u64,
    /// The most the instants of `tsc` and `utc_ns` can differ, in
    /// nanoseconds.
    pub pairing_ns: u64,
    /// The status of host UTC.
    pub status: HostClockStatus,
    /// A counter that changes when host UTC steps, when the host resumes
    /// from a suspension, and when the guest TSC's offset from the host TSC
    /// changes.
    pub disruption: u32,
    /// How fast host UTC runs against the guest TSC counted at the declared
    /// rate `F`, minus one, in parts per million scaled by 2^16.
    pub correction: i64,
    /// A bound on the error of `correction`, in parts per million scaled by
    /// 2^16, unless the host's clock may not follow the TSC.
    pub correction_bound: Option<u64>,
    /// The time service's bound on the error of host UTC, in microseconds.
    pub max_error_us: Option<u64>,
}

/// Returns how fast host UTC runs against the TSC counted at `declared_hz`,
/// minus one, in parts per million scaled by 2^16, rounded to the nearest,
/// for a host whose UTC runs `host_correction` faster than its uncorrected
/// clock, which counts the TSC at `native_hz`, and whose published UTC the
/// rate hook makes run `rate_offset_ppm` faster still.
///
/// That is `(1 + host_correction) * F / F_d * (1 + rate_offset) - 1`: on a
/// restore, it includes the deviation of the native rate from the declared
/// one, which the restore packet reports.
pub fn sample_correction(
    host_correction: i64,
    declared_hz: u64,
    native_hz: u64,
    rate_offset_ppm: i32,
) -> i64 {
    let million = 1_000_000;
    let native_hz = i128::from(native_hz.max(1));
    let numerator = (ONE + i128::from(host_correction))
        * i128::from(declared_hz)
        * (million + i128::from(rate_offset_ppm))
        - ONE * native_hz * million;
    let denominator = native_hz * million;
    let rounded = if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    };
    rounded.clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// Returns a bound on the error of a published correction, in parts per
/// million scaled by 2^16, for a host that derives UTC from `host` and a
/// backend whose native rate `native_hz` measures `native`, or `None` if the
/// host's clock may not follow the TSC at all.
///
/// When the two rates come from the same calibration, only the rounding
/// remains; otherwise one is the other to the kHz.
pub fn correction_bound(
    host: HostClockBasis,
    native: NativeRateBasis,
    native_hz: u64,
) -> Option<u64> {
    match (host, native) {
        (HostClockBasis::KernelTsc, NativeRateBasis::KernelKhz)
        | (HostClockBasis::HypervisorTsc, NativeRateBasis::Hypervisor) => Some(CORRECTION_ROUNDING),
        (HostClockBasis::KernelTsc | HostClockBasis::HypervisorTsc, _) => {
            let khz = (1_000 * ONE as u128).div_ceil(u128::from(native_hz.max(1)));
            Some(u64::try_from(khz).unwrap_or(u64::MAX) + CORRECTION_ROUNDING)
        }
        (HostClockBasis::Other, _) => None,
    }
}

/// The host clock readings that the disruption check compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockMarks {
    utc_ns: u64,
    monotonic_ns: u64,
    suspended_ns: u64,
    correction: i64,
}

/// Returns whether host UTC stepped, or the host was suspended, between the
/// readings `last` and `now` of a host whose monotonic clock is `clock`.
fn disrupted(last: &ClockMarks, now: &ClockMarks, clock: HostClockKind) -> bool {
    let threshold = i128::from(DISRUPTION_THRESHOLD_NS);
    if now.suspended_ns.abs_diff(last.suspended_ns) >= DISRUPTION_THRESHOLD_NS {
        return true;
    }
    let Some(elapsed) = now.monotonic_ns.checked_sub(last.monotonic_ns) else {
        return true;
    };
    let elapsed = i128::from(elapsed);
    let utc_elapsed = i128::from(now.utc_ns) - i128::from(last.utc_ns);
    let (expected, tolerance) = if clock.follows_utc_rate() {
        (elapsed, threshold)
    } else {
        // The host corrects UTC but not the monotonic clock, and the
        // correction may have changed since the last sample.
        (
            elapsed + elapsed * i128::from(now.correction) / ONE,
            threshold + elapsed * CORRECTION_CHANGE_PPM / 1_000_000,
        )
    };
    (utc_elapsed - expected).abs() >= tolerance
}

/// Publishes the host's side of the time samples of one VM process.
#[derive(Debug)]
pub struct HostClockPublisher {
    /// The host monotonic clock that the readings carry.
    clock: HostClockKind,
    /// The `utc-offset-ms` hook, in nanoseconds.
    utc_offset_ns: i128,
    /// The `host-rate-offset-ppm` hook.
    rate_offset_ppm: i32,
    /// The host UTC of the first sample, from which the rate hook skews
    /// published UTC.
    skew_origin_ns: Option<u64>,
    /// The readings of the last sample.
    last: Option<ClockMarks>,
    /// The disruption counter.
    disruption: u32,
}

impl HostClockPublisher {
    /// Returns a publisher for a host whose monotonic clock is `clock`, with
    /// the test hooks `hooks`.
    pub fn new(clock: HostClockKind, hooks: &TimeAbiTestHooks) -> Self {
        Self {
            clock,
            utc_offset_ns: i128::from(hooks.utc_offset_ms) * 1_000_000,
            rate_offset_ppm: hooks.host_rate_offset_ppm,
            skew_origin_ns: None,
            last: None,
            disruption: 0,
        }
    }

    /// Records a disruption that the host clock readings cannot show, such
    /// as a change of the guest TSC's offset from the host TSC.
    pub fn disrupt(&mut self) {
        self.disruption = self.disruption.wrapping_add(1);
    }

    /// Returns the host's fields of a time sample from `readings`, with the
    /// guest TSC's offset `offset` and the rates `rates`.
    pub fn publish(
        &mut self,
        readings: &HostReadings,
        offset: GuestTscOffset,
        rates: SampleRates,
    ) -> PublishedSample {
        let marks = ClockMarks {
            utc_ns: readings.time.utc_ns,
            monotonic_ns: readings.time.monotonic_ns,
            suspended_ns: readings.suspended_ns,
            correction: readings.discipline.correction,
        };
        if let Some(last) = self.last.replace(marks)
            && disrupted(&last, &marks, self.clock)
        {
            self.disrupt();
        }
        let utc_ns = readings.time.utc_ns;
        let origin = *self.skew_origin_ns.get_or_insert(utc_ns);
        let skew = (i128::from(utc_ns) - i128::from(origin)) * i128::from(self.rate_offset_ppm)
            / 1_000_000;
        let utc_ns = (i128::from(utc_ns) + skew + self.utc_offset_ns).clamp(1, u64::MAX.into());
        PublishedSample {
            utc_ns: utc_ns as u64,
            tsc: offset.guest_tsc(readings.host_tsc),
            pairing_ns: readings
                .pairing_ns
                .saturating_add(cycles_to_ns_ceil(offset.uncertainty, rates.native_hz)),
            status: readings.discipline.status,
            disruption: self.disruption,
            correction: sample_correction(
                readings.discipline.correction,
                rates.declared_hz,
                rates.native_hz,
                self.rate_offset_ppm,
            ),
            correction_bound: correction_bound(
                readings.discipline.basis,
                rates.native_basis,
                rates.native_hz,
            ),
            max_error_us: readings.discipline.max_error_us,
        }
    }

    /// Takes the host's fields of a time sample now: pairs host UTC with the
    /// host TSC and reads the discipline of host UTC.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    pub fn sample(
        &mut self,
        offset: GuestTscOffset,
        rates: SampleRates,
    ) -> Result<PublishedSample, TimeAbiError> {
        let (host_tsc, time, pairing_ns) = pair_host_time(rates.native_hz)?;
        let readings = HostReadings {
            host_tsc,
            time,
            pairing_ns,
            discipline: super::host::host_clock_discipline(),
            suspended_ns: super::host::host_suspended_ns()?,
        };
        Ok(self.publish(&readings, offset, rates))
    }
}

/// Host time pairings that a sample tries.
const PAIRING_ATTEMPTS: usize = 16;

/// A pairing this tight, in nanoseconds, ends the attempts early.
const PAIRING_TARGET_NS: u64 = 1_000;

/// Pairs a host time sample with the host TSC, keeping the tightest of
/// [`PAIRING_ATTEMPTS`] pairings, and returns the host TSC, the sample, and
/// their pairing uncertainty in nanoseconds at `tsc_hz`.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "x86_64")]
fn pair_host_time(tsc_hz: u64) -> Result<(u64, HostTimeSample, u64), TimeAbiError> {
    use super::tsc::HostTscBracket;
    use super::tsc::pair_host_sample;

    let mut tightest: Option<(u64, HostTimeSample, u64)> = None;
    for _ in 0..PAIRING_ATTEMPTS {
        let (bracket, time) = HostTscBracket::around(super::host::sample_host_time);
        let time = time?;
        let Some((host_tsc, pairing_ns)) = pair_host_sample(bracket, tsc_hz) else {
            continue;
        };
        if tightest.is_none_or(|(_, _, tightest)| pairing_ns < tightest) {
            tightest = Some((host_tsc, time, pairing_ns));
        }
        if pairing_ns <= PAIRING_TARGET_NS {
            break;
        }
    }
    tightest.ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::TscAnchor,
            "the host TSC went backwards around every host time sample",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PPM: i64 = SCALED_PPM;

    fn discipline(correction: i64) -> HostClockDiscipline {
        HostClockDiscipline {
            status: HostClockStatus::Synchronized,
            correction,
            max_error_us: Some(1_500),
            basis: HostClockBasis::KernelTsc,
        }
    }

    fn readings(utc_ns: u64, monotonic_ns: u64, correction: i64) -> HostReadings {
        HostReadings {
            host_tsc: 1_000_000,
            time: HostTimeSample {
                utc_ns,
                monotonic_ns,
            },
            pairing_ns: 40,
            discipline: discipline(correction),
            suspended_ns: 0,
        }
    }

    const RATES: SampleRates = SampleRates {
        declared_hz: 2_000_000_000,
        native_hz: 2_000_000_000,
        native_basis: NativeRateBasis::KernelKhz,
    };

    const OFFSET: GuestTscOffset = GuestTscOffset {
        offset: 500,
        uncertainty: 2_000,
    };

    #[test]
    fn corrections_fold_the_declared_rate_and_the_hook() {
        let f = 2_000_000_000;
        assert_eq!(sample_correction(0, f, f, 0), 0);
        assert_eq!(sample_correction(-12 * PPM, f, f, 0), -12 * PPM);
        // A native rate 100 ppm above the declared one: the guest's clock
        // runs 99.99 ppm fast unless it corrects.
        assert_eq!(sample_correction(0, f, f + 200_000, 0), -6_552_945);
        // The hook multiplies: (1 + 100 ppm)(1 + 200 ppm) - 1.
        assert_eq!(sample_correction(100 * PPM, f, f, 200), 300 * PPM + 1_311);
        assert_eq!(sample_correction(0, f, f, -50), -50 * PPM);
        // Linux's widest tick and Windows' adjustments stay in range.
        assert_eq!(sample_correction(100_000 * PPM, f, f, 0), 100_000 * PPM);
    }

    #[test]
    fn bounds_depend_on_the_rates_calibration() {
        let hz = 2_621_440_000;
        assert_eq!(
            correction_bound(HostClockBasis::KernelTsc, NativeRateBasis::KernelKhz, hz),
            Some(CORRECTION_ROUNDING)
        );
        assert_eq!(
            correction_bound(
                HostClockBasis::HypervisorTsc,
                NativeRateBasis::Hypervisor,
                hz
            ),
            Some(CORRECTION_ROUNDING)
        );
        // One kHz of 2.62144 GHz is 0.381... ppm, or 25,000 scaled units.
        for (host, native) in [
            (HostClockBasis::HypervisorTsc, NativeRateBasis::KernelKhz),
            (HostClockBasis::KernelTsc, NativeRateBasis::Hypervisor),
        ] {
            assert_eq!(
                correction_bound(host, native, hz),
                Some(25_000 + CORRECTION_ROUNDING)
            );
        }
        assert_eq!(
            correction_bound(HostClockBasis::Other, NativeRateBasis::KernelKhz, hz),
            None
        );
    }

    #[test]
    fn samples_pair_utc_with_the_guest_tsc() {
        let mut publisher =
            HostClockPublisher::new(HostClockKind::LinuxBoottime, &TimeAbiTestHooks::default());
        let sample = publisher.publish(
            &readings(1_700_000_000_000_000_000, 5_000, 0),
            OFFSET,
            RATES,
        );
        assert_eq!(
            sample,
            PublishedSample {
                utc_ns: 1_700_000_000_000_000_000,
                tsc: 1_000_500,
                // 40 ns of pairing and 2,000 cycles, 1 µs, of offset.
                pairing_ns: 1_040,
                status: HostClockStatus::Synchronized,
                disruption: 0,
                correction: 0,
                correction_bound: Some(CORRECTION_ROUNDING),
                max_error_us: Some(1_500),
            }
        );
    }

    #[test]
    fn linux_steps_and_suspensions_are_disruptions() {
        fn publish(
            publisher: &mut HostClockPublisher,
            utc_ns: u64,
            monotonic_ns: u64,
            suspended_ns: u64,
        ) -> u32 {
            let mut readings = readings(utc_ns, monotonic_ns, 25 * PPM);
            readings.suspended_ns = suspended_ns;
            publisher.publish(&readings, OFFSET, RATES).disruption
        }

        let mut publisher =
            HostClockPublisher::new(HostClockKind::LinuxBoottime, &TimeAbiTestHooks::default());
        let p = &mut publisher;
        let start = 1_700_000_000_000_000_000;
        assert_eq!(publish(p, start, 1_000_000_000, 0), 0);
        // Both clocks advance together, whatever the correction.
        assert_eq!(publish(p, start + 64_000_000_000, 65_000_000_000, 0), 0);
        // A step just under the threshold, then one at it.
        assert_eq!(publish(p, start + 128_000_999_999, 129_000_000_000, 0), 0);
        assert_eq!(publish(p, start + 192_001_999_999, 193_000_000_000, 0), 1);
        // A suspension of 30 s advances both clocks and the suspended time.
        assert_eq!(
            publish(p, start + 286_001_999_999, 287_000_000_000, 30_000_000_000),
            2
        );
        // A monotonic clock that goes backwards is a disruption too.
        assert_eq!(
            publish(p, start + 286_001_999_999, 286_000_000_000, 30_000_000_000),
            3
        );
        p.disrupt();
        assert_eq!(
            publish(p, start + 287_001_999_999, 287_000_000_000, 30_000_000_000),
            4
        );
    }

    #[test]
    fn windows_utc_may_run_at_its_correction() {
        let mut publisher = HostClockPublisher::new(
            HostClockKind::WindowsInterruptTime,
            &TimeAbiTestHooks::default(),
        );
        let start = 1_700_000_000_000_000_000;
        let mut publish = |utc_ns, monotonic_ns, correction| {
            publisher
                .publish(&readings(utc_ns, monotonic_ns, correction), OFFSET, RATES)
                .disruption
        };
        assert_eq!(publish(start, 1_000_000_000, 100 * PPM), 0);
        // 64 s of interrupt time at a correction of 100 ppm: 6.4 ms more UTC.
        assert_eq!(
            publish(start + 64_006_400_000, 65_000_000_000, 100 * PPM),
            0
        );
        // Within the 32 ms that a correction change could explain.
        assert_eq!(publish(start + 128_030_000_000, 129_000_000_000, 0), 0);
        // A 40 ms step is not.
        assert_eq!(publish(start + 192_070_000_000, 193_000_000_000, 0), 1);
    }

    #[test]
    fn hooks_offset_and_skew_published_utc() {
        let hooks = TimeAbiTestHooks {
            utc_offset_ms: -5,
            host_rate_offset_ppm: 250,
            ..Default::default()
        };
        let mut publisher = HostClockPublisher::new(HostClockKind::LinuxBoottime, &hooks);
        let start = 1_700_000_000_000_000_000;
        let first = publisher.publish(&readings(start, 1_000_000_000, 0), OFFSET, RATES);
        assert_eq!(first.utc_ns, start - 5_000_000);
        assert_eq!(first.correction, 250 * PPM);
        // 100 s later, published UTC is 25 ms further ahead, and the skew is
        // no disruption.
        let later = publisher.publish(
            &readings(start + 100_000_000_000, 101_000_000_000, 0),
            OFFSET,
            RATES,
        );
        assert_eq!(
            later.utc_ns,
            start + 100_000_000_000 + 25_000_000 - 5_000_000
        );
        assert_eq!(later.disruption, 0);
    }

    #[test]
    fn published_utc_is_never_zero() {
        let hooks = TimeAbiTestHooks {
            utc_offset_ms: -10,
            ..Default::default()
        };
        let mut publisher = HostClockPublisher::new(HostClockKind::LinuxBoottime, &hooks);
        let sample = publisher.publish(&readings(1_000, 0, 0), OFFSET, RATES);
        assert_eq!(sample.utc_ns, 1);
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
    #[test]
    fn host_samples_pair_tightly() {
        let mut publisher = HostClockPublisher::new(
            super::super::host::host_clock_kind(),
            &TimeAbiTestHooks::default(),
        );
        let sample = publisher
            .sample(
                GuestTscOffset {
                    offset: 0,
                    uncertainty: 0,
                },
                RATES,
            )
            .unwrap();
        // After 2020-01-01.
        assert!(sample.utc_ns > 1_577_836_800_000_000_000);
        assert!(sample.pairing_ns < 100_000, "{sample:?}");
    }
}
