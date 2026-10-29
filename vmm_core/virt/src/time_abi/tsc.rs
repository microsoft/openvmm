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

/// Pairs a host operation with the bracket of host TSC reads around it.
///
/// Returns the host TSC at the middle of the bracket and the pairing
/// uncertainty, half the bracket, in nanoseconds at `tsc_hz`. `None` if the
/// TSC went backwards.
pub fn pair_host_sample(bracket: HostTscBracket, tsc_hz: u64) -> Option<(u64, u64)> {
    let (host_tsc, half_width) = bracket.midpoint()?;
    Some((host_tsc, cycles_to_ns_ceil(half_width, tsc_hz)))
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

}
