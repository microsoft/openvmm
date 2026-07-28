// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Enumeration of virtio-villain tests and parsing of their serial verdicts.

use anyhow::Context as _;
use std::path::Path;

/// A single villain test, as described by one row of `tests.tsv`.
///
/// `tests.tsv` is emitted by `init --list-tsv` (see villain `bin/init.c`) with
/// one tab-separated row per test and no header:
///
/// ```text
/// name  desc  version  spec_section  device_id  flags  required_features  min_queues
/// ```
#[derive(Debug, Clone)]
pub struct VillainTest {
    /// Test identifier, passed to the guest via `vv.test=<name>` and matched
    /// against the serial verdict marker.
    pub name: String,
    /// Human-readable description.
    pub desc: String,
    /// virtio spec version the test targets (`tests.tsv` column 3, free text,
    /// e.g. `"1.2"`).
    pub version: String,
    /// virtio spec section the test references (`tests.tsv` column 4, e.g.
    /// `"2.6.5"`).
    pub spec_section: String,
    /// Virtio PCI device ID the test targets (derived from
    /// `virtio_spec::pci::VIRTIO_PCI_DEVICE_ID_BASE` and
    /// `virtio_spec::VirtioDeviceType`).
    pub device_id: u16,
    /// Test flags bitfield (`tests.tsv` column 6; see villain `tests/test.h`
    /// `TEST_FLAG_*`). Only [`TEST_FLAG_MMIO`] currently affects the harness.
    pub flags: u8,
    /// virtio feature bits the test requires (`tests.tsv` column 7). Not yet
    /// consumed by the harness beyond logging.
    pub required_features: u64,
    /// Minimum virtqueue count the test needs (`tests.tsv` column 8). Not yet
    /// consumed by the harness beyond logging.
    pub min_queues: u32,
}

/// villain `TEST_FLAG_MMIO` (`tests/test.h`): the test drives the virtio-MMIO
/// transport rather than PCI, so the VM must attach its devices on the MMIO bus
/// (otherwise the guest finds no MMIO device and the test self-`[SKIP]`s).
pub const TEST_FLAG_MMIO: u8 = 0x2;

impl VillainTest {
    /// Whether this test targets the virtio-MMIO transport (`TEST_FLAG_MMIO`).
    ///
    /// Such tests must be run in a VM whose virtio devices are attached over
    /// the MMIO bus; see [`crate::run::run_one`].
    pub fn is_mmio(&self) -> bool {
        self.flags & TEST_FLAG_MMIO != 0
    }
}

/// Parse `tests.tsv` into the list of villain tests.
pub fn parse_tsv(path: &Path) -> anyhow::Result<Vec<VillainTest>> {
    let text = fs_err::read_to_string(path)?;
    let mut tests = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut cols = line.split('\t');
        let name = cols
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("{}:{}: missing test name", path.display(), lineno + 1))?;
        // Columns: name, desc, version, spec_section, device_id, flags,
        // required_features, min_queues. Parse strictly so a corrupt or
        // format-changed tests.tsv fails loudly rather than silently producing
        // bogus values.
        let desc = cols.next().with_context(|| {
            format!(
                "{}:{}: missing description column",
                path.display(),
                lineno + 1
            )
        })?;
        let version = cols.next().filter(|s| !s.is_empty()).with_context(|| {
            format!("{}:{}: missing version column", path.display(), lineno + 1)
        })?;
        let spec_section = cols.next().filter(|s| !s.is_empty()).with_context(|| {
            format!(
                "{}:{}: missing spec_section column",
                path.display(),
                lineno + 1
            )
        })?;
        let device_id = cols.next().with_context(|| {
            format!(
                "{}:{}: missing device_id column",
                path.display(),
                lineno + 1
            )
        })?;
        let device_id = device_id
            .strip_prefix("0x")
            .and_then(|h| u16::from_str_radix(h, 16).ok())
            .with_context(|| {
                format!(
                    "{}:{}: invalid device_id {:?} (expected 0x-prefixed u16 hex)",
                    path.display(),
                    lineno + 1,
                    device_id,
                )
            })?;
        let flags = cols
            .next()
            .with_context(|| format!("{}:{}: missing flags column", path.display(), lineno + 1))?;
        let flags = flags.parse::<u8>().with_context(|| {
            format!(
                "{}:{}: invalid flags {:?} (expected decimal u8)",
                path.display(),
                lineno + 1,
                flags,
            )
        })?;
        // required_features (col 7) and min_queues (col 8) are not yet consumed
        // by the harness beyond being logged at the start of each test, but
        // validate their presence and format so a column added or removed after
        // `flags` fails loudly rather than silently shifting the remaining
        // fields.
        let required_features = cols.next().with_context(|| {
            format!(
                "{}:{}: missing required_features column",
                path.display(),
                lineno + 1
            )
        })?;
        let required_features = required_features
            .strip_prefix("0x")
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .with_context(|| {
                format!(
                    "{}:{}: invalid required_features {:?} (expected 0x-prefixed u64 hex)",
                    path.display(),
                    lineno + 1,
                    required_features,
                )
            })?;
        let min_queues = cols.next().with_context(|| {
            format!(
                "{}:{}: missing min_queues column",
                path.display(),
                lineno + 1
            )
        })?;
        let min_queues = min_queues.parse::<u32>().with_context(|| {
            format!(
                "{}:{}: invalid min_queues {:?} (expected decimal integer)",
                path.display(),
                lineno + 1,
                min_queues,
            )
        })?;

        // min_queues (col 8) is the last column villain's `list_tests_tsv`
        // emits. Reject any trailing column so a schema addition fails loudly
        // here rather than being silently dropped.
        if let Some(extra) = cols.next() {
            anyhow::bail!(
                "{}:{}: unexpected trailing column {:?} after min_queues \
                 (tests.tsv schema changed?)",
                path.display(),
                lineno + 1,
                extra,
            );
        }

        tests.push(VillainTest {
            name: name.to_string(),
            desc: desc.to_string(),
            version: version.to_string(),
            spec_section: spec_section.to_string(),
            device_id,
            flags,
            required_features,
            min_queues,
        });
    }
    anyhow::ensure!(!tests.is_empty(), "no tests found in {}", path.display());
    Ok(tests)
}

/// The verdict the guest emits on the serial console for a test.
///
/// The guest prints `[<TAG>] <name>` (villain `bin/init.c`), where `<TAG>` is
/// one of these values. The device-health re-probe that distinguishes REJECT
/// from WEDGED is computed by the guest itself; the host only reads the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Device accepted the (valid) input and behaved correctly.
    Pass,
    /// Device correctly rejected the malformed input and stayed alive.
    Reject,
    /// Device absent or precondition unmet; the test did not run.
    Skip,
    /// Device accepted malformed input / misbehaved.
    Fail,
    /// Device stopped responding after the malformed input.
    Wedged,
    /// Known-failing test that failed as expected (guest-side xfail).
    Xfail,
    /// A test flagged as a known xfail that unexpectedly passed. Villain treats
    /// this as a *failure* (`verdict_failed()` in `bin/init.c`): an XPASS means
    /// the underlying bug is gone and the stale xfail marker must be removed.
    Xpass,
}

impl Verdict {
    fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "PASS" => Verdict::Pass,
            "REJECT" => Verdict::Reject,
            "SKIP" => Verdict::Skip,
            "FAIL" => Verdict::Fail,
            "WEDGED" => Verdict::Wedged,
            "XFAIL" => Verdict::Xfail,
            "XPASS" => Verdict::Xpass,
            _ => return None,
        })
    }

    /// Whether this verdict is a "good" outcome (the device model behaved
    /// correctly or it is a guest-side xfail — neither of which indicates an
    /// OpenVMM device-model bug).
    ///
    /// Note `Verdict::Skip` is deliberately **not** good: on the kitchen-sink VM
    /// a skip means the device was absent or a precondition was unmet, so the
    /// test exercised nothing. That is always a failure; tests for devices we do
    /// not attach are `#[ignore]`d up front (see [`crate::supported_devices`]).
    ///
    /// `Verdict::Xpass` is likewise **not** good: villain only emits it for an
    /// xfail-flagged test that unexpectedly passed, which it counts as a failure
    /// so the stale marker gets noticed and removed. Treating it as success here
    /// would silently mask that signal.
    pub fn is_good(self) -> bool {
        matches!(self, Verdict::Pass | Verdict::Reject | Verdict::Xfail)
    }
}

/// The result of scanning a serial log for a single test's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictScan {
    /// The test's own `[TAG] <name>` marker was found.
    Found(Verdict),
    /// No villain output appeared at all — not even the startup banner — so
    /// the guest never reached villain (e.g. it failed to boot).
    NoMarkers,
    /// Villain ran (its startup banner or other tests' markers appeared) but
    /// this test's verdict line never printed — the device wedged or killed
    /// the guest mid-test before emitting a verdict.
    MarkerMissing,
}

/// Incremental scanner for villain serial verdict markers.
#[derive(Default)]
pub struct VerdictScanner {
    saw_any_marker: bool,
    villain_started: bool,
}

impl VerdictScanner {
    /// Scans one serial log line.
    ///
    /// Returns `Some` as soon as `name`'s verdict marker is found; otherwise
    /// the caller should continue scanning and call [`Self::finish`] at EOF.
    pub fn scan_line(&mut self, line: &str, name: &str) -> Option<VerdictScan> {
        let line = line.trim();
        // Markers look like "[PASS] blk.split.bad_desc". Find the closing
        // bracket and split into tag + remainder.
        let rest = line.strip_prefix('[')?;
        let (tag, remainder) = rest.split_once(']')?;
        let Some(verdict) = Verdict::from_tag(tag) else {
            // Not a verdict tag. Villain's `[vv] ...` startup banner proves it
            // booted and began running, so even if it later wedges before this
            // test's verdict we can report `MarkerMissing` (started, no verdict)
            // rather than `NoMarkers` (never ran).
            if tag == "vv" {
                self.villain_started = true;
            }
            return None;
        };
        self.saw_any_marker = true;
        // The name is the first whitespace-delimited token after the tag.
        // Some markers carry a trailing reason, e.g.
        // "[SKIP] D0001 (no device 0x1063)", so match only the first token
        // rather than the whole remainder. Villain test names never contain
        // whitespace (see `tests.tsv`).
        (remainder.split_whitespace().next() == Some(name)).then_some(VerdictScan::Found(verdict))
    }

    /// Returns the scan result once EOF is reached without finding `name`.
    pub fn finish(self) -> VerdictScan {
        if self.saw_any_marker || self.villain_started {
            VerdictScan::MarkerMissing
        } else {
            VerdictScan::NoMarkers
        }
    }
}

/// Scan serial console log lines for `name`'s verdict marker.
pub fn scan_verdict_lines<'a>(lines: impl IntoIterator<Item = &'a str>, name: &str) -> VerdictScan {
    let mut scanner = VerdictScanner::default();
    for line in lines {
        if let Some(scan) = scanner.scan_line(line, name) {
            return scan;
        }
    }
    scanner.finish()
}

/// Scan a captured serial console log for `name`'s verdict marker.
///
/// Villain prints one `[<TAG>] <name>` line per test. A test may share the log
/// with unrelated boot output; we match the exact test name.
pub fn scan_verdict(log: &str, name: &str) -> VerdictScan {
    scan_verdict_lines(log.lines(), name)
}

/// Turn a scan result into a pass/fail outcome for one villain test.
///
/// Fast-fail rule: a `SKIP` means the device was absent or a precondition was
/// unmet, so the test exercised nothing — always a **failure** (an unexercised
/// test must not masquerade as a pass). Tests for devices the kitchen-sink VM
/// does not attach are `#[ignore]`d up front (see [`crate::supported_devices`])
/// so they never reach here in a normal run; force-running them with
/// `--run-ignored` correctly reports the absent-device skip as a failure.
///
/// Otherwise [`Verdict::is_good`] decides, and a missing/absent marker (the
/// guest wedged or never booted) is a failure.
pub fn evaluate(scan: VerdictScan) -> anyhow::Result<()> {
    match scan {
        VerdictScan::Found(Verdict::Skip) => anyhow::bail!(
            "SKIP: the device was absent or a precondition was unmet, so this test \
             exercised nothing. On the kitchen-sink VM a skip means a device we meant \
             to attach silently wasn't (a harness/config bug); if the device is \
             deliberately not attached, the test should be ignored via \
             supported_devices::expected_skip"
        ),
        VerdictScan::Found(v) => {
            if v.is_good() {
                Ok(())
            } else {
                anyhow::bail!("{v:?}")
            }
        }
        VerdictScan::MarkerMissing => {
            anyhow::bail!("WEDGED (no verdict marker for this test)")
        }
        VerdictScan::NoMarkers => {
            anyhow::bail!("FAIL (guest emitted no villain markers)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Use the tracing-aware `#[test]` so `tracing` output appears in test logs
    // (repo convention; see .github/copilot-instructions.md).
    use test_with_tracing::test;

    #[test]
    fn evaluate_skip_is_always_a_failure() {
        // SKIP means nothing was exercised. Unsupported-device tests are ignored
        // up front; anything that actually runs and skips is a failure.
        assert!(evaluate(VerdictScan::Found(Verdict::Skip)).is_err());
    }

    #[test]
    fn evaluate_good_and_bad_verdicts() {
        assert!(evaluate(VerdictScan::Found(Verdict::Pass)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Reject)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Xfail)).is_ok());
        // XPASS is a failure by villain's own semantics (stale xfail marker).
        assert!(evaluate(VerdictScan::Found(Verdict::Xpass)).is_err());
        assert!(evaluate(VerdictScan::Found(Verdict::Fail)).is_err());
        assert!(evaluate(VerdictScan::Found(Verdict::Wedged)).is_err());
        assert!(evaluate(VerdictScan::MarkerMissing).is_err());
        assert!(evaluate(VerdictScan::NoMarkers).is_err());
    }

    #[test]
    fn scan_finds_exact_marker() {
        let log = "\
[vv] virtio-villain
[SKIP] blk.other
[PASS] blk.split.bad_desc
";
        assert_eq!(
            scan_verdict(log, "blk.split.bad_desc"),
            VerdictScan::Found(Verdict::Pass)
        );
        assert_eq!(
            scan_verdict(log, "blk.other"),
            VerdictScan::Found(Verdict::Skip)
        );
    }

    #[test]
    fn scan_marker_with_trailing_reason() {
        // Villain appends a reason to some SKIP markers; the name is still the
        // first token (bin/init.c: "[SKIP] %s (no device 0x%04x)").
        let log = "[vv] virtio-villain\n[SKIP] D0001 (no device 0x1063)\n";
        assert_eq!(
            scan_verdict(log, "D0001"),
            VerdictScan::Found(Verdict::Skip)
        );
        // A name that is a prefix of the marked test must not match.
        assert_eq!(scan_verdict(log, "D000"), VerdictScan::MarkerMissing);
    }

    #[test]
    fn scan_banner_only_is_marker_missing() {
        // Villain printed its startup banner but wedged before emitting any
        // verdict: the guest *ran* villain, so this is MarkerMissing, not
        // NoMarkers (which means villain never started).
        let log = "booting...\n[vv] virtio-villain\nkernel panic\n";
        assert_eq!(scan_verdict(log, "M0030"), VerdictScan::MarkerMissing);
    }

    #[test]
    fn scan_missing_vs_no_markers() {
        assert_eq!(scan_verdict("[FAIL] a\n", "b"), VerdictScan::MarkerMissing);
        assert_eq!(scan_verdict("booting...\n", "b"), VerdictScan::NoMarkers);
    }

    #[test]
    fn parse_tsv_row() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        std::fs::write(
            &p,
            "blk.split.bad_desc\tvalidates descriptor\t1.2\t2.6.5\t0x1042\t0\t0x0000000000000000\t1\n",
        )
        .unwrap();
        let tests = parse_tsv(&p).unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name, "blk.split.bad_desc");
        assert_eq!(tests[0].device_id, 0x1042);
        assert_eq!(tests[0].flags, 0);
        assert!(!tests[0].is_mmio());
    }

    #[test]
    fn parse_tsv_mmio_flag() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // An MMIO-transport test: device-agnostic (0x0000) with flags == 2
        // (TEST_FLAG_MMIO), exactly as villain emits for the `M####` tests.
        std::fs::write(
            &p,
            "M0001\tNon-32-bit access to MMIO control registers\t1.2\t4.2.2.2\t0x0000\t2\t0x0000000000000000\t0\n",
        )
        .unwrap();
        let tests = parse_tsv(&p).unwrap();
        assert_eq!(tests[0].flags, TEST_FLAG_MMIO);
        assert!(tests[0].is_mmio());
    }

    #[test]
    fn parse_tsv_rejects_bad_flags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // flags column present but not a decimal u8.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\t0x0002\tnope\t0\t1\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("invalid flags"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_bad_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // device_id column present but not valid 0x-prefixed hex.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\tnope\t0\t0\t1\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("invalid device_id"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_truncated_row() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // Has name/desc/version/spec_section but the device_id column is missing.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("missing device_id"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_missing_trailing_columns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // name..flags present (6 cols) but required_features/min_queues absent.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\t0x1042\t0\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("missing required_features"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_bad_min_queues() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // min_queues column present but not a decimal integer.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\t0x1042\t0\t0x0\tnope\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("invalid min_queues"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_trailing_column() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // All 8 columns present plus an unexpected 9th column.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\t0x1042\t0\t0x0\t1\textra\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("unexpected trailing column"),
            "unexpected error: {err:#}"
        );
    }
}
