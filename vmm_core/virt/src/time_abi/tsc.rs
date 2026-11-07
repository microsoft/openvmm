// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host TSC reads, which pair the host's clocks and the guest TSC with an
//! instant on the host TSC.

// UNSAFETY: Executing LFENCE, which orders the host TSC reads.
// xtask-fmt allow-target-arch cpu-intrinsic
#![cfg_attr(target_arch = "x86_64", expect(unsafe_code))]

/// Two host TSC reads around another host operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTscBracket {
    /// The host TSC before the operation.
    pub start: u64,
    /// The host TSC after the operation.
    pub end: u64,
}

/// Waits until every earlier instruction has completed locally, so that a
/// TSC read is not reordered with its neighbors.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "x86_64")]
fn lfence() {
    // SAFETY: LFENCE only orders instruction execution.
    unsafe { core::arch::asm!("lfence", options(nostack, preserves_flags)) };
}

impl HostTscBracket {
    /// Runs `f` between two host TSC reads.
    ///
    /// Each read waits for every earlier instruction to complete, so that
    /// neither is reordered with `f`.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    pub fn around<T>(f: impl FnOnce() -> T) -> (Self, T) {
        lfence();
        let start = safe_intrinsics::rdtsc();
        lfence();
        let value = f();
        lfence();
        let end = safe_intrinsics::rdtsc();
        (Self { start, end }, value)
    }

    /// The host TSC at the middle of the bracket, and the most the instant of
    /// the bracketed operation can be from it, in cycles. `None` if the TSC
    /// went backwards.
    pub fn midpoint(&self) -> Option<(u64, u64)> {
        let width = self.end.checked_sub(self.start)?;
        Some((self.start + width / 2, width.div_ceil(2)))
    }
}

/// Converts `cycles` at `tsc_hz` to nanoseconds, rounding up.
pub fn cycles_to_ns_ceil(cycles: u64, tsc_hz: u64) -> u64 {
    let ns = (u128::from(cycles) * 1_000_000_000).div_ceil(u128::from(tsc_hz.max(1)));
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// Converts `ns` nanoseconds to cycles at `tsc_hz`, rounding down.
pub fn ns_to_cycles(ns: u64, tsc_hz: u64) -> u64 {
    let cycles = u128::from(ns) * u128::from(tsc_hz) / 1_000_000_000;
    u64::try_from(cycles).unwrap_or(u64::MAX)
}

/// Pairs a host operation with the bracket of host TSC reads around it.
///
/// Returns the host TSC at the middle of the bracket and the pairing
/// uncertainty, half the bracket, in nanoseconds at `tsc_hz`. `None` if the
/// TSC went backwards.
pub fn pair_host_sample(bracket: HostTscBracket, tsc_hz: u64) -> Option<(u64, u64)> {
    let (host_tsc, half_width) = bracket.midpoint()?;
    Some((host_tsc, cycles_to_ns_ceil(half_width, tsc_hz)))
}

/// Guest TSC reads that a backend's offset measurement tries.
pub const OFFSET_ATTEMPTS: usize = 8;

/// An offset measurement this tight, in cycles, ends the reads early.
pub const OFFSET_TARGET_CYCLES: u64 = 2_000;

/// An offset measurement whose tightest reading is still wider than this, in
/// nanoseconds, after [`OFFSET_ATTEMPTS`] reads, reads on. Every time sample
/// adds the offset's uncertainty to its pairing, which the guest requires
/// within 100 µs, and on a nested host every register read can take
/// milliseconds for a while when other partitions run.
pub const OFFSET_WIDE_NS: u64 = 50_000;

/// Guest TSC reads that a backend's offset measurement takes at most, as many
/// as a capture anchor tries.
pub const OFFSET_MAX_ATTEMPTS: usize = 64;

/// The offset of a guest's TSC from the host TSC that this process reads.
///
/// The guest TSC is never scaled, so the offset holds until the VMM writes a
/// guest TSC again, or stops and resumes partition time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTscOffset {
    /// The guest TSC is the host TSC plus this offset, wrapping.
    pub offset: u64,
    /// The most the true offset can differ from `offset`, in cycles.
    pub uncertainty: u64,
}

impl GuestTscOffset {
    /// Returns the guest TSC at the host TSC `host_tsc`.
    pub fn guest_tsc(&self, host_tsc: u64) -> u64 {
        host_tsc.wrapping_add(self.offset)
    }

    /// Returns the offset of a guest TSC that was `guest_tsc` within
    /// `bracket`, or `None` if the host TSC went backwards.
    pub fn from_bracket(guest_tsc: u64, bracket: HostTscBracket) -> Option<Self> {
        let (host_tsc, uncertainty) = bracket.midpoint()?;
        Some(Self {
            offset: guest_tsc.wrapping_sub(host_tsc),
            uncertainty,
        })
    }

    /// Returns whether `other` can be the same offset, within both
    /// uncertainties.
    pub fn agrees_with(&self, other: &Self) -> bool {
        // The offsets wrap, so their difference is the shorter way round.
        let difference = self.offset.wrapping_sub(other.offset) as i64;
        difference.unsigned_abs() <= self.uncertainty.saturating_add(other.uncertainty)
    }

    /// Measures the offset of a guest TSC that `read` returns, reading it
    /// between two host TSC reads, and keeps the tightest reading. It reads
    /// `attempts` times, stopping early at an uncertainty of `target` cycles,
    /// and then reads on while the tightest reading is wider than `wide`
    /// cycles, up to `limit` reads in all. Returns `None` if the host TSC went
    /// backwards around every read.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    pub fn measure<E>(
        attempts: usize,
        target: u64,
        wide: u64,
        limit: usize,
        mut read: impl FnMut() -> Result<u64, E>,
    ) -> Result<Option<Self>, E> {
        let mut tightest: Option<Self> = None;
        for count in 0..attempts.max(limit) {
            if count >= attempts && tightest.is_some_and(|tightest| tightest.uncertainty <= wide) {
                break;
            }
            let (bracket, guest_tsc) = HostTscBracket::around(&mut read);
            let Some(candidate) = Self::from_bracket(guest_tsc?, bracket) else {
                continue;
            };
            if tightest.is_none_or(|tightest| candidate.uncertainty < tightest.uncertainty) {
                tightest = Some(candidate);
            }
            if candidate.uncertainty <= target {
                break;
            }
        }
        Ok(tightest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_conversion_rounds_up() {
        assert_eq!(cycles_to_ns_ceil(0, 2_000_000_000), 0);
        assert_eq!(cycles_to_ns_ceil(1, 2_000_000_000), 1);
        // One millisecond at the bare-metal KVM host's rate, and a hair over a
        // microsecond.
        assert_eq!(cycles_to_ns_ceil(2_194_804, 2_194_804_000), 1_000_000);
        assert_eq!(cycles_to_ns_ceil(2_195, 2_194_804_000), 1_001);
        assert_eq!(cycles_to_ns_ceil(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn nanosecond_conversion_rounds_down() {
        assert_eq!(ns_to_cycles(0, 2_000_000_000), 0);
        // The wide offset at the 2.79 GHz rate of the nested WHP runners.
        assert_eq!(ns_to_cycles(OFFSET_WIDE_NS, 2_793_440_195), 139_672);
        assert_eq!(ns_to_cycles(1, 999_999_999), 0);
        assert_eq!(ns_to_cycles(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn brackets_pair_at_their_midpoint() {
        let bracket = HostTscBracket {
            start: 1_000,
            end: 1_005,
        };
        assert_eq!(bracket.midpoint(), Some((1_002, 3)));
        assert_eq!(pair_host_sample(bracket, 1_000_000_000), Some((1_002, 3)));
        assert_eq!(HostTscBracket { start: 10, end: 9 }.midpoint(), None);
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn host_tsc_reads_advance() {
        let (bracket, ()) = HostTscBracket::around(|| ());
        assert!(bracket.end >= bracket.start, "{bracket:?}");
    }

    #[test]
    fn offsets_pair_the_guest_tsc_with_the_bracket_midpoint() {
        let bracket = HostTscBracket {
            start: 1_000,
            end: 1_010,
        };
        let offset = GuestTscOffset::from_bracket(5_000, bracket).unwrap();
        assert_eq!(
            offset,
            GuestTscOffset {
                offset: 3_995,
                uncertainty: 5
            }
        );
        assert_eq!(offset.guest_tsc(2_005), 6_000);
        // A guest TSC below the host TSC wraps.
        let behind = GuestTscOffset::from_bracket(5, bracket).unwrap();
        assert_eq!(behind.guest_tsc(1_005), 5);
        assert_eq!(
            GuestTscOffset::from_bracket(0, HostTscBracket { start: 10, end: 9 }),
            None
        );
    }

    #[test]
    fn offsets_agree_within_their_uncertainties() {
        let offset = |offset: u64, uncertainty| GuestTscOffset {
            offset,
            uncertainty,
        };
        assert!(offset(1_000, 5).agrees_with(&offset(1_010, 5)));
        assert!(!offset(1_000, 5).agrees_with(&offset(1_011, 5)));
        assert!(offset(0, 0).agrees_with(&offset(0, 0)));
        // Across the wrap.
        assert!(offset(u64::MAX, 1).agrees_with(&offset(0, 0)));
        assert!(!offset(u64::MAX - 1, 0).agrees_with(&offset(1, 1)));
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn measurement_keeps_the_tightest_reading() {
        // A guest TSC that runs one million cycles ahead of the host TSC.
        let read = || Ok::<_, ()>(safe_intrinsics::rdtsc().wrapping_add(1_000_000));
        let offset = GuestTscOffset::measure(8, 0, u64::MAX, 8, read)
            .unwrap()
            .unwrap();
        // The read lies between the two host reads, so the offset is within
        // the uncertainty of one million cycles.
        assert!(
            offset.agrees_with(&GuestTscOffset {
                offset: 1_000_000,
                uncertainty: 0
            }),
            "{offset:?}"
        );
        assert_eq!(
            GuestTscOffset::measure(4, 0, u64::MAX, 4, || Err::<u64, _>(7)),
            Err(7)
        );
        assert_eq!(GuestTscOffset::measure(0, 0, u64::MAX, 0, read), Ok(None));
    }

    /// The cycles for which a stalled read of the guest TSC stalls.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    const STALL_CYCLES: u64 = 400_000;

    /// The width of a wide measurement in the tests: a stalled read is wider.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    const WIDE_CYCLES: u64 = 100_000;

    /// Measures the offset of a guest TSC that equals the host TSC, whose
    /// first `stalled` reads take [`STALL_CYCLES`] each, as a nested host's
    /// register reads can for a while, and returns the offset and the number
    /// of reads.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    fn measure_stalled(stalled: usize) -> (GuestTscOffset, usize) {
        let mut reads = 0;
        let offset =
            GuestTscOffset::measure(OFFSET_ATTEMPTS, 0, WIDE_CYCLES, OFFSET_MAX_ATTEMPTS, || {
                reads += 1;
                if reads <= stalled {
                    let start = safe_intrinsics::rdtsc();
                    while safe_intrinsics::rdtsc().wrapping_sub(start) < STALL_CYCLES {}
                }
                Ok::<_, ()>(safe_intrinsics::rdtsc())
            })
            .unwrap()
            .unwrap();
        (offset, reads)
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn measurement_reads_on_while_its_reads_are_wide() {
        // Every read of the first attempts stalls, so their tightest reading is
        // wide; the measurement reads on until a read is no longer wide.
        let (offset, reads) = measure_stalled(OFFSET_ATTEMPTS);
        assert!(offset.uncertainty <= WIDE_CYCLES, "{offset:?}");
        assert!(
            (OFFSET_ATTEMPTS + 1..=OFFSET_MAX_ATTEMPTS).contains(&reads),
            "{reads} reads"
        );
        assert!(offset.agrees_with(&GuestTscOffset {
            offset: 0,
            uncertainty: 0
        }));
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn measurement_takes_its_attempts_when_one_is_not_wide() {
        // Only the first read stalls: the tightest of the attempts is not wide,
        // so the measurement takes no further read.
        let (offset, reads) = measure_stalled(1);
        assert!(offset.uncertainty <= WIDE_CYCLES, "{offset:?}");
        assert_eq!(reads, OFFSET_ATTEMPTS);
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn measurement_stops_at_its_limit() {
        // Every read stalls: the measurement keeps the tightest of them all.
        let (offset, reads) = measure_stalled(usize::MAX);
        assert!(offset.uncertainty > WIDE_CYCLES, "{offset:?}");
        assert_eq!(reads, OFFSET_MAX_ATTEMPTS);
    }
}
