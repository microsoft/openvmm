// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host TSC reads, which pair the host's clocks and the guest TSC with an
//! instant on the host TSC.

// UNSAFETY: Executing LFENCE, which orders the host TSC reads.
// xtask-fmt allow-target-arch cpu-intrinsic
#![cfg_attr(target_arch = "x86_64", expect(unsafe_code))]

use super::TimeAbiCode;
use super::TimeAbiError;

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

/// Guest TSC reads that a backend's offset measurement takes at least.
pub const OFFSET_ATTEMPTS: usize = 8;

/// An offset measurement this tight, in cycles, ends the reads early.
pub const OFFSET_TARGET_CYCLES: u64 = 2_000;

/// The widest offset uncertainty, in nanoseconds, that a start accepts: half
/// the 100 µs within which the guest accepts a time sample, whose pairing
/// uncertainty includes the offset's. Once its first [`OFFSET_ATTEMPTS`]
/// reads leave the offset wider, a measurement reads on until a read is
/// within it.
pub const OFFSET_BOUND_NS: u64 = 50_000;

/// How long, in nanoseconds of the host TSC from its first read, an offset
/// measurement reads on while its tightest offset is wider than
/// [`OFFSET_BOUND_NS`], before the start fails with `E_TSC_ANCHOR`.
///
/// No number of reads is enough on its own: on a nested host, tearing down
/// another VM's memory stalls every hypercall for milliseconds at a time, in
/// bursts that can span a second or more, so a measurement takes as long as
/// the stall lasts. The offset holds until the next start, so every time
/// sample until then would carry an offset measured during such a stall.
/// This bounds only a host that never recovers.
pub const OFFSET_DEADLINE_NS: u64 = 10_000_000_000;

/// The most guest TSC reads that an offset measurement takes, whatever the
/// host TSC says of their time: more than [`OFFSET_DEADLINE_NS`] admits of
/// reads wider than [`OFFSET_BOUND_NS`], each of which takes at least twice
/// the bound.
pub const OFFSET_MAX_READS: usize = 1 << 17;

/// How an offset measurement reads a guest TSC: at least `attempts` times,
/// ending early once a read is within `target` cycles, and then on while its
/// tightest offset is wider than `bound` cycles, until `deadline` cycles of
/// the host TSC have passed since its first read began or it has read
/// `max_reads` times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetSearch {
    /// The reads that the measurement takes at least.
    pub attempts: usize,
    /// An uncertainty, in cycles, that ends the reads at once.
    pub target: u64,
    /// The widest uncertainty, in cycles, that ends the reads after
    /// `attempts`.
    pub bound: u64,
    /// The host TSC cycles after which the reads end, however wide.
    pub deadline: u64,
    /// The most reads that the measurement takes.
    pub max_reads: usize,
}

impl OffsetSearch {
    /// Returns the search of the offset constants, with the bound and the
    /// deadline in cycles of a host TSC that runs at `tsc_hz`.
    pub fn at_rate(tsc_hz: u64) -> Self {
        Self {
            attempts: OFFSET_ATTEMPTS,
            target: OFFSET_TARGET_CYCLES,
            bound: ns_to_cycles(OFFSET_BOUND_NS, tsc_hz),
            deadline: ns_to_cycles(OFFSET_DEADLINE_NS, tsc_hz),
            max_reads: OFFSET_MAX_READS,
        }
    }
}

/// A measured guest TSC offset, with the reads that it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetMeasurement {
    /// The tightest offset that the reads found.
    pub offset: GuestTscOffset,
    /// The number of reads.
    pub reads: usize,
    /// The host TSC cycles from the start of the first read to the end of
    /// the last.
    pub cycles: u64,
}

impl OffsetMeasurement {
    /// Logs the measurement, of a host TSC that runs at `tsc_hz`.
    pub fn log(&self, tsc_hz: u64) {
        tracing::info!(
            offset = format_args!("{:#x}", self.offset.offset),
            uncertainty_cycles = self.offset.uncertainty,
            reads = self.reads,
            elapsed_us = cycles_to_ns_ceil(self.cycles, tsc_hz) / 1_000,
            "time ABI: guest TSC offset"
        );
    }

    /// Returns the measured offset if it is within the bound of `search`, of
    /// a host TSC that runs at `tsc_hz`, and otherwise fails with
    /// `E_TSC_ANCHOR`: every time sample until the next start would carry the
    /// offset's uncertainty, and the guest accepts none over its bound.
    pub fn within_bound(
        &self,
        search: OffsetSearch,
        tsc_hz: u64,
    ) -> Result<GuestTscOffset, TimeAbiError> {
        if self.offset.uncertainty <= search.bound {
            return Ok(self.offset);
        }
        Err(TimeAbiError::new(
            TimeAbiCode::TscAnchor,
            format!(
                "VP 0's TSC offset stayed wider than {} ns for {} ms: the tightest of {} \
                 reads paired it within {} ns",
                cycles_to_ns_ceil(search.bound, tsc_hz),
                cycles_to_ns_ceil(self.cycles, tsc_hz) / 1_000_000,
                self.reads,
                cycles_to_ns_ceil(self.offset.uncertainty, tsc_hz),
            ),
        ))
    }
}

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
    /// between two host TSC reads as `search` says, and keeps the tightest
    /// reading. Returns `None` if the host TSC went backwards around every
    /// read.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    pub fn measure<E>(
        search: OffsetSearch,
        mut read: impl FnMut() -> Result<u64, E>,
    ) -> Result<Option<OffsetMeasurement>, E> {
        Self::search(search, || {
            let (bracket, guest_tsc) = HostTscBracket::around(&mut read);
            Ok((bracket, guest_tsc?))
        })
    }

    /// Measures the offset of the guest TSC reads that `bracketed` returns,
    /// each with the host TSC bracket around it, as `search` says, and keeps
    /// the tightest. Returns `None` if the host TSC went backwards around
    /// every read.
    ///
    /// The measurement takes the time of its reads from their brackets, so a
    /// host TSC that went backwards since its first read ends it once it has
    /// taken its attempts.
    pub fn search<E>(
        search: OffsetSearch,
        mut bracketed: impl FnMut() -> Result<(HostTscBracket, u64), E>,
    ) -> Result<Option<OffsetMeasurement>, E> {
        let mut tightest: Option<Self> = None;
        let mut first_start = None;
        let mut last_end = 0;
        let mut reads = 0;
        let elapsed = |first_start: Option<u64>, last_end: u64| {
            first_start.map_or(0, |start| last_end.wrapping_sub(start))
        };
        while reads < search.max_reads {
            if reads >= search.attempts
                && (tightest.is_none_or(|tightest| tightest.uncertainty <= search.bound)
                    || elapsed(first_start, last_end) >= search.deadline)
            {
                break;
            }
            let (bracket, guest_tsc) = bracketed()?;
            reads += 1;
            first_start.get_or_insert(bracket.start);
            last_end = bracket.end;
            let Some(candidate) = Self::from_bracket(guest_tsc, bracket) else {
                continue;
            };
            if tightest.is_none_or(|tightest| candidate.uncertainty < tightest.uncertainty) {
                tightest = Some(candidate);
            }
            if candidate.uncertainty <= search.target {
                break;
            }
        }
        Ok(tightest.map(|offset| OffsetMeasurement {
            offset,
            reads,
            cycles: elapsed(first_start, last_end),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

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
        // The offset bound at the 2.79 GHz rate of the nested WHP runners.
        assert_eq!(ns_to_cycles(OFFSET_BOUND_NS, 2_793_440_195), 139_672);
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
        let eight = OffsetSearch {
            attempts: 8,
            target: 0,
            bound: u64::MAX,
            deadline: 0,
            max_reads: 8,
        };
        let measured = GuestTscOffset::measure(eight, read).unwrap().unwrap();
        // The read lies between the two host reads, so the offset is within
        // the uncertainty of one million cycles.
        assert!(
            measured.offset.agrees_with(&GuestTscOffset {
                offset: 1_000_000,
                uncertainty: 0
            }),
            "{measured:?}"
        );
        assert_eq!(measured.reads, 8);
        assert_eq!(GuestTscOffset::measure(eight, || Err::<u64, _>(7)), Err(7));
        let none = OffsetSearch {
            attempts: 0,
            ..eight
        };
        assert_eq!(GuestTscOffset::measure(none, read), Ok(None));
    }

    /// The offset constants at a 2.8 GHz host TSC: a bound of 50 µs and a
    /// deadline of 10 s.
    const SEARCH: OffsetSearch = OffsetSearch {
        attempts: 8,
        target: 2_000,
        bound: 140_000,
        deadline: 28_000_000_000,
        max_reads: OFFSET_MAX_READS,
    };

    /// The offset of the guest TSC that [`reads`] returns.
    const OFFSET: u64 = 5_000;

    /// Returns reads of a guest TSC [`OFFSET`] cycles ahead of a host TSC
    /// that advances by each bracket's width, with brackets of `widths`, in
    /// order.
    fn reads(
        widths: impl IntoIterator<Item = u64>,
    ) -> impl FnMut() -> Result<(HostTscBracket, u64), ()> {
        let mut widths = widths.into_iter();
        let mut now = 1_000_000;
        move || {
            let width = widths.next().expect("the search read past its brackets");
            let bracket = HostTscBracket {
                start: now,
                end: now + width,
            };
            now += width;
            Ok((bracket, bracket.start + width / 2 + OFFSET))
        }
    }

    #[test]
    fn the_search_scales_its_bound_and_deadline_to_the_host_tsc() {
        assert_eq!(
            OffsetSearch::at_rate(2_793_440_000),
            OffsetSearch {
                attempts: OFFSET_ATTEMPTS,
                target: OFFSET_TARGET_CYCLES,
                bound: 139_672,
                deadline: 27_934_400_000,
                max_reads: OFFSET_MAX_READS,
            }
        );
    }

    #[test]
    fn an_offset_within_its_bound_takes_the_attempts() {
        // A nested host's read takes about 20 µs, 56,000 cycles.
        let measured = GuestTscOffset::search(SEARCH, reads([56_000; 8]))
            .unwrap()
            .unwrap();
        assert_eq!(
            measured,
            OffsetMeasurement {
                offset: GuestTscOffset {
                    offset: OFFSET,
                    uncertainty: 28_000,
                },
                reads: 8,
                cycles: 8 * 56_000,
            }
        );
    }

    #[test]
    fn a_read_within_the_target_ends_the_search() {
        let measured = GuestTscOffset::search(SEARCH, reads([56_000, 4_000]))
            .unwrap()
            .unwrap();
        assert_eq!(measured.reads, 2);
        assert_eq!(measured.offset.uncertainty, 2_000);
    }

    #[test]
    fn the_search_reads_on_through_a_stall() {
        // A stall slows each of the first 20 reads to 600,000 cycles, which
        // would leave the offset 300,000 cycles wide, 107 µs at 2.8 GHz; the
        // next read is quick again.
        let widths = std::iter::repeat_n(600_000, 20).chain([56_000]);
        let measured = GuestTscOffset::search(SEARCH, reads(widths))
            .unwrap()
            .unwrap();
        assert_eq!(
            measured,
            OffsetMeasurement {
                offset: GuestTscOffset {
                    offset: OFFSET,
                    uncertainty: 28_000,
                },
                reads: 21,
                cycles: 20 * 600_000 + 56_000,
            }
        );
    }

    #[test]
    fn no_number_of_slow_reads_ends_the_search() {
        // On a nested host, tearing down another VM's memory slowed one start's
        // reads to about 5.4 ms each, 15,000,000 cycles, for 143 reads, 0.77 s,
        // before a read was quick again. A cap on the reads would have kept an
        // offset of 2.7 ms.
        let widths = std::iter::repeat_n(15_000_000, 143).chain([56_000]);
        let measured = GuestTscOffset::search(SEARCH, reads(widths))
            .unwrap()
            .unwrap();
        assert_eq!(measured.reads, 144);
        assert_eq!(measured.cycles, 143 * 15_000_000 + 56_000);
        assert_eq!(
            measured.within_bound(SEARCH, 2_800_000_000),
            Ok(GuestTscOffset {
                offset: OFFSET,
                uncertainty: 28_000,
            })
        );
    }

    #[test]
    fn the_search_ends_at_its_deadline() {
        let search = OffsetSearch {
            deadline: 6_000_000,
            ..SEARCH
        };
        // The reads stay wide, and the tenth ends the 6,000,000 cycles.
        let measured = GuestTscOffset::search(search, reads(std::iter::repeat(600_000)))
            .unwrap()
            .unwrap();
        assert_eq!(measured.reads, 10);
        assert_eq!(measured.cycles, 6_000_000);
        assert_eq!(measured.offset.uncertainty, 300_000);
    }

    #[test]
    fn a_start_accepts_only_an_offset_within_the_bound() {
        let tight = GuestTscOffset::search(SEARCH, reads([56_000; 8]))
            .unwrap()
            .unwrap();
        assert_eq!(
            tight.within_bound(SEARCH, 2_800_000_000),
            Ok(GuestTscOffset {
                offset: OFFSET,
                uncertainty: 28_000,
            })
        );
        // A stall outlasts the deadline: the start fails rather than give
        // every time sample a 107 µs offset.
        let search = OffsetSearch {
            deadline: 6_000_000,
            ..SEARCH
        };
        let wide = GuestTscOffset::search(search, reads(std::iter::repeat(600_000)))
            .unwrap()
            .unwrap();
        assert_eq!(
            wide.within_bound(search, 2_800_000_000),
            Err(TimeAbiError::new(
                TimeAbiCode::TscAnchor,
                "VP 0's TSC offset stayed wider than 50000 ns for 2 ms: the tightest of 10 \
                 reads paired it within 107143 ns",
            ))
        );
    }

    #[test]
    fn the_search_takes_at_most_its_maximum_reads() {
        let search = OffsetSearch {
            max_reads: 100,
            ..SEARCH
        };
        // A host TSC that never advances past the first bracket.
        let stuck = || {
            let bracket = HostTscBracket {
                start: 1_000_000,
                end: 1_600_000,
            };
            Ok::<_, ()>((bracket, 1_300_000 + OFFSET))
        };
        let measured = GuestTscOffset::search(search, stuck).unwrap().unwrap();
        assert_eq!(measured.reads, 100);
        assert_eq!(measured.offset.uncertainty, 300_000);
    }

    #[test]
    fn a_host_tsc_that_went_backwards_ends_the_search() {
        let mut first = true;
        let backwards = || {
            let start = if std::mem::take(&mut first) {
                1_000_000
            } else {
                10
            };
            let bracket = HostTscBracket {
                start,
                end: start + 600_000,
            };
            Ok::<_, ()>((bracket, start + 300_000 + OFFSET))
        };
        let measured = GuestTscOffset::search(SEARCH, backwards).unwrap().unwrap();
        assert_eq!(measured.reads, SEARCH.attempts);
        // Every bracket around a read that went backwards gives no offset.
        let inverted = || {
            Ok::<_, ()>((
                HostTscBracket {
                    start: 1_000,
                    end: 999,
                },
                OFFSET,
            ))
        };
        assert_eq!(GuestTscOffset::search(SEARCH, inverted), Ok(None));
        assert_eq!(
            GuestTscOffset::search(SEARCH, || Err::<(HostTscBracket, u64), _>(7)),
            Err(7)
        );
    }
}
