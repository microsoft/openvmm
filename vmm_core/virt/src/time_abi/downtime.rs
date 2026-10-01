// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Downtime selection and the restore TSC target.

use super::HostIdentity;
use super::HostTimeSample;
use super::TimeAbiCode;
use super::TimeAbiError;
use super::TimeAbiTestHooks;

/// The longest accepted downtime: 30 days.
pub const MAX_DOWNTIME_NS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;
/// The smallest disagreement between the UTC delta and a monotonic downtime
/// that is reported as a host wall-clock step.
pub const HOST_WALL_CLOCK_STEP_NS: u64 = 1_000_000_000;

/// What capture recorded at the capture anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTimeRecord {
    /// `T_c`, the guest TSC of VP 0.
    pub tsc: u64,
    /// The host clocks.
    pub sample: HostTimeSample,
    /// The host and boot identities.
    pub identity: HostIdentity,
}

/// The source of a downtime measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowntimeSource {
    /// Host monotonic time on the same host boot.
    HostMonotonic,
    /// The host UTC delta.
    Utc,
}

/// A selected downtime `D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downtime {
    /// `D`, in nanoseconds.
    pub nanos: u64,
    /// The source `D` came from.
    pub source: DowntimeSource,
    /// On the monotonic path, the UTC delta minus the monotonic delta when
    /// it exceeds [`HOST_WALL_CLOCK_STEP_NS`] in magnitude: the host
    /// wall clock was stepped. It is logged and does not change `D`.
    pub host_wall_clock_step_ns: Option<i64>,
}

/// Selects the downtime source and measures `D` from the capture record to
/// the destination sample `now`.
///
/// Host monotonic time is used iff the host identity, boot identity, and
/// clock kind all match and no test hook forces UTC; otherwise the UTC delta
/// is used. Test hooks apply as specified: `utc_offset_ms` to the destination
/// UTC reading and `downtime_add_s` to `D` before the bounds check.
pub fn select_downtime(
    capture: &CaptureTimeRecord,
    destination: &HostIdentity,
    now: &HostTimeSample,
    hooks: &TimeAbiTestHooks,
) -> Result<Downtime, TimeAbiError> {
    let utc_now = i128::from(now.utc_ns) + i128::from(hooks.utc_offset_ms) * 1_000_000;
    let utc_delta = utc_now - i128::from(capture.sample.utc_ns);
    let same_boot = capture.identity.host_id == destination.host_id
        && capture.identity.boot_id == destination.boot_id
        && capture.identity.clock == destination.clock
        && !hooks.boot_id_mismatch;

    let (source, measured, host_wall_clock_step_ns) = if same_boot && !hooks.force_utc_downtime {
        let delta = i128::from(now.monotonic_ns) - i128::from(capture.sample.monotonic_ns);
        let step = utc_delta - delta;
        let step = (step.unsigned_abs() > u128::from(HOST_WALL_CLOCK_STEP_NS))
            .then(|| i64::try_from(step).unwrap_or(if step < 0 { i64::MIN } else { i64::MAX }));
        (DowntimeSource::HostMonotonic, delta, step)
    } else {
        (DowntimeSource::Utc, utc_delta, None)
    };

    let downtime = measured + i128::from(hooks.downtime_add_s) * 1_000_000_000;
    if downtime < 0 {
        return Err(TimeAbiError::new(
            TimeAbiCode::DowntimeNegative,
            format!("downtime from the {source:?} source is negative: {downtime} ns"),
        ));
    }
    if downtime > i128::from(MAX_DOWNTIME_NS) {
        return Err(TimeAbiError::new(
            TimeAbiCode::DowntimeExcessive,
            format!(
                "downtime from the {source:?} source is {downtime} ns, above {MAX_DOWNTIME_NS} ns"
            ),
        ));
    }
    Ok(Downtime {
        nanos: downtime as u64,
        source,
        host_wall_clock_step_ns,
    })
}

/// Returns the restore TSC target `T_c + floor(D * F_s / 1_000_000_000)`.
pub fn tsc_target(capture_tsc: u64, downtime_ns: u64, tsc_hz: u64) -> Result<u64, TimeAbiError> {
    let cycles = u128::from(downtime_ns) * u128::from(tsc_hz) / 1_000_000_000;
    u64::try_from(u128::from(capture_tsc) + cycles).map_err(|_| {
        TimeAbiError::new(
            TimeAbiCode::TscTargetOverflow,
            format!(
                "TSC target from {capture_tsc} plus {downtime_ns} ns at {tsc_hz} Hz exceeds 64 bits"
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_abi::HostClockKind;

    const SECOND: u64 = 1_000_000_000;

    fn identity(host: u8, boot: u8) -> HostIdentity {
        HostIdentity {
            host_id: [host; 16],
            boot_id: [boot; 16],
            clock: HostClockKind::LinuxBoottime,
        }
    }

    fn capture() -> CaptureTimeRecord {
        CaptureTimeRecord {
            tsc: 1_000,
            sample: HostTimeSample {
                utc_ns: 1_700_000_000 * SECOND,
                monotonic_ns: 100 * SECOND,
            },
            identity: identity(1, 1),
        }
    }

    fn after(seconds_utc: u64, seconds_monotonic: u64) -> HostTimeSample {
        let capture = capture();
        HostTimeSample {
            utc_ns: capture.sample.utc_ns + seconds_utc * SECOND,
            monotonic_ns: capture.sample.monotonic_ns + seconds_monotonic * SECOND,
        }
    }

    fn select(
        destination: &HostIdentity,
        now: &HostTimeSample,
        hooks: &TimeAbiTestHooks,
    ) -> Result<Downtime, TimeAbiError> {
        select_downtime(&capture(), destination, now, hooks)
    }

    #[test]
    fn same_boot_uses_host_monotonic_time() {
        let downtime = select(&identity(1, 1), &after(7, 5), &Default::default()).unwrap();
        assert_eq!(
            downtime,
            Downtime {
                nanos: 5 * SECOND,
                source: DowntimeSource::HostMonotonic,
                host_wall_clock_step_ns: Some(2 * SECOND as i64),
            }
        );
        let downtime = select(&identity(1, 1), &after(6, 5), &Default::default()).unwrap();
        assert_eq!(downtime.host_wall_clock_step_ns, None);
    }

    #[test]
    fn any_identity_difference_uses_utc() {
        let mut other_clock = identity(1, 1);
        other_clock.clock = HostClockKind::WindowsInterruptTime;
        for destination in [identity(2, 1), identity(1, 2), other_clock] {
            let downtime = select(&destination, &after(7, 0), &Default::default()).unwrap();
            assert_eq!(downtime.source, DowntimeSource::Utc);
            assert_eq!(downtime.nanos, 7 * SECOND);
            assert_eq!(downtime.host_wall_clock_step_ns, None);
        }
    }

    #[test]
    fn hooks_force_utc() {
        for hooks in [
            TimeAbiTestHooks {
                force_utc_downtime: true,
                ..Default::default()
            },
            TimeAbiTestHooks {
                boot_id_mismatch: true,
                ..Default::default()
            },
        ] {
            let downtime = select(&identity(1, 1), &after(7, 5), &hooks).unwrap();
            assert_eq!(downtime.source, DowntimeSource::Utc);
            assert_eq!(downtime.nanos, 7 * SECOND);
        }
    }

    #[test]
    fn bounds() {
        let max_seconds = MAX_DOWNTIME_NS / SECOND;
        let downtime =
            select(&identity(2, 2), &after(max_seconds, 0), &Default::default()).unwrap();
        assert_eq!(downtime.nanos, MAX_DOWNTIME_NS);
        assert_eq!(
            select(
                &identity(2, 2),
                &after(max_seconds + 1, 0),
                &Default::default()
            )
            .unwrap_err()
            .code,
            TimeAbiCode::DowntimeExcessive
        );
        let hooks = TimeAbiTestHooks {
            downtime_add_s: 2_592_001,
            ..Default::default()
        };
        assert_eq!(
            select(&identity(1, 1), &after(0, 0), &hooks)
                .unwrap_err()
                .code,
            TimeAbiCode::DowntimeExcessive
        );
    }

    #[test]
    fn negative_downtime_is_rejected() {
        let mut now = after(0, 0);
        now.utc_ns -= 1;
        assert_eq!(
            select(&identity(2, 2), &now, &Default::default())
                .unwrap_err()
                .code,
            TimeAbiCode::DowntimeNegative
        );
        let mut now = after(0, 0);
        now.monotonic_ns -= 1;
        assert_eq!(
            select(&identity(1, 1), &now, &Default::default())
                .unwrap_err()
                .code,
            TimeAbiCode::DowntimeNegative
        );
        let hooks = TimeAbiTestHooks {
            force_utc_downtime: true,
            utc_offset_ms: -10_000,
            ..Default::default()
        };
        assert_eq!(
            select(&identity(1, 1), &after(5, 5), &hooks)
                .unwrap_err()
                .code,
            TimeAbiCode::DowntimeNegative
        );
    }

    #[test]
    fn utc_offset_hook_applies_to_utc() {
        let hooks = TimeAbiTestHooks {
            utc_offset_ms: 1_500,
            ..Default::default()
        };
        let downtime = select(&identity(2, 2), &after(5, 0), &hooks).unwrap();
        assert_eq!(downtime.nanos, 6_500_000_000);
    }

    #[test]
    fn target() {
        assert_eq!(
            tsc_target(1_000, SECOND, 2_000_000_000).unwrap(),
            2_000_001_000
        );
        // Floors partial cycles: 1 ns at 2.5 GHz is 2.5 cycles.
        assert_eq!(tsc_target(0, 1, 2_500_000_000).unwrap(), 2);
        assert_eq!(
            tsc_target(u64::MAX - 1, 1, 2_000_000_000).unwrap_err().code,
            TimeAbiCode::TscTargetOverflow
        );
        assert_eq!(
            tsc_target(u64::MAX - 2, 1, 2_000_000_000).unwrap(),
            u64::MAX
        );
    }
}
