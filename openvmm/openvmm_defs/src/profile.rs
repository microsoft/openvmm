// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Opt-in snapshot lifecycle profiling shared by OpenVMM components.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// Environment variable that enables snapshot lifecycle profiling.
pub const SNAPSHOT_PROFILE_ENV: &str = "OPENVMM_STARTUP_PROFILE";

/// Prefix for machine-readable snapshot lifecycle profile records.
pub const SNAPSHOT_PROFILE_PREFIX: &str = "OPENVMM_SNAPSHOT_PROFILE_V1";

static ENABLED: OnceLock<bool> = OnceLock::new();
static PROCESS_STARTED: OnceLock<Instant> = OnceLock::new();

/// Optional counters associated with a completed lifecycle phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProfileCounters {
    /// Logical artifact bytes handled by the phase.
    pub logical_bytes: Option<u64>,
    /// Physically allocated artifact bytes handled by the phase.
    pub allocated_bytes: Option<u64>,
    /// Guest-physical memory faults observed through the VMM resolver.
    pub gpa_faults: Option<u64>,
    /// Bytes populated while resolving guest-physical memory faults.
    pub populated_bytes: Option<u64>,
}

/// An optional monotonic timer that is inert unless profiling is enabled.
#[derive(Debug)]
pub struct ProfileSpan {
    started: Option<Instant>,
}

/// Initializes the process-relative profile clock when profiling is enabled.
///
/// Call this as early as possible during process startup. Repeated calls are
/// harmless.
pub fn initialize() {
    if enabled() {
        PROCESS_STARTED.get_or_init(Instant::now);
    }
}

/// Returns whether snapshot lifecycle profiling is enabled.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os(SNAPSHOT_PROFILE_ENV)
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}

impl ProfileSpan {
    /// Starts an interval only when profiling is enabled.
    pub fn start() -> Self {
        let started = enabled().then(|| {
            let now = Instant::now();
            PROCESS_STARTED.get_or_init(|| now);
            now
        });
        Self { started }
    }

    /// Emits one exclusive phase interval.
    pub fn complete(self, operation: &str, phase: &str, counters: ProfileCounters) {
        self.emit(operation, phase, true, counters);
    }

    /// Emits one cumulative milestone that may contain nested phase intervals.
    pub fn complete_milestone(self, operation: &str, phase: &str, counters: ProfileCounters) {
        self.emit(operation, phase, false, counters);
    }

    fn emit(self, operation: &str, phase: &str, exclusive: bool, counters: ProfileCounters) {
        let Some(started) = self.started else {
            return;
        };
        let ended = Instant::now();
        let process_started = PROCESS_STARTED.get().copied().unwrap_or(started);
        let line = format_record(
            operation,
            phase,
            exclusive,
            ended.duration_since(started),
            ended.duration_since(process_started),
            std::process::id(),
            counters,
        );
        // Profiling must never change VM behavior. In particular, a closed
        // diagnostic stream is not a lifecycle failure.
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }
}

fn format_record(
    operation: &str,
    phase: &str,
    exclusive: bool,
    duration: Duration,
    process_elapsed: Duration,
    pid: u32,
    counters: ProfileCounters,
) -> String {
    let mut line = format!(
        "{SNAPSHOT_PROFILE_PREFIX} operation={operation} phase={phase} exclusive={} \
         duration_ns={} process_elapsed_ns={} pid={pid}",
        u8::from(exclusive),
        duration.as_nanos(),
        process_elapsed.as_nanos(),
    );
    if let Some(value) = counters.logical_bytes {
        let _ = write!(line, " logical_bytes={value}");
    }
    if let Some(value) = counters.allocated_bytes {
        let _ = write!(line, " allocated_bytes={value}");
    }
    if let Some(value) = counters.gpa_faults {
        let _ = write!(line, " gpa_faults={value}");
    }
    if let Some(value) = counters.populated_bytes {
        let _ = write!(line, " populated_bytes={value}");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::ProfileCounters;
    use super::format_record;
    use std::time::Duration;

    #[test]
    fn profile_record_has_stable_machine_readable_fields() {
        assert_eq!(
            format_record(
                "restore",
                "artifact_open",
                true,
                Duration::from_nanos(12),
                Duration::from_nanos(34),
                56,
                ProfileCounters {
                    logical_bytes: Some(78),
                    allocated_bytes: Some(90),
                    gpa_faults: None,
                    populated_bytes: None,
                },
            ),
            "OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore phase=artifact_open \
             exclusive=1 duration_ns=12 process_elapsed_ns=34 pid=56 \
             logical_bytes=78 allocated_bytes=90"
        );
    }
}
