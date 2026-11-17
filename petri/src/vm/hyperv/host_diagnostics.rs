// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-side hypervisor diagnostics for Hyper-V SNP VMs, recorded for the
//! lifetime of the VM into the test output directory.
//!
//! The trace records the hypervisor diagnostics TraceLogging provider, which
//! reports the hardware exits and software paths behind
//! `HvMessageTypeUnrecoverableException`. The hypervisor buffers these events
//! in a circular buffer and `hvservice` forwards them to ETW on capture-state
//! requests, so a helper process sends those requests periodically and once
//! more before the trace stops. The events are only emitted by instrumented
//! hypervisor builds, so the host's hypervisor images are recorded as well.
//!
//! Traces use host-wide ETW sessions, so they also contain events for any
//! other VMs running concurrently on the host.

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// `Microsoft.Windows.HyperV.Hypervisor.Diagnostics`.
const HYPERVISOR_DIAGNOSTICS_PROVIDER: &str = "{f1c134a0-5523-5865-1aff-b9f82e911cc3}";

const FILE_STEM: &str = "hv_snp_diag";
const CAPTURE_STATE_SCRIPT: &str = include_str!("etw_capture_state.ps1");
const CAPTURE_STATE_INTERVAL_SECS: u32 = 5;
const CAPTURE_STATE_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

const SESSION_PREFIX: &str = "petri-hv-";
/// Matches the maximum number of concurrently running VMM tests.
const MAX_PETRI_SESSIONS: usize = 16;
/// Leave room below the default system-wide limit of 64 ETW sessions so that
/// tracing never starves other components on the host.
const MAX_TOTAL_SESSIONS: usize = 56;
const MAX_TRACE_MB: u32 = 256;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

/// Records hypervisor diagnostics until dropped.
pub(super) struct HostDiagnostics {
    session: Option<String>,
    capture_state: Option<CaptureState>,
    output_dir: PathBuf,
}

impl HostDiagnostics {
    /// Starts collecting diagnostics into `output_dir`.
    ///
    /// Failures are logged and leave collection disabled; they never fail the
    /// test.
    pub fn start(output_dir: &Path) -> Self {
        let session = start_trace(output_dir)
            .inspect_err(|e| {
                tracing::warn!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "failed to start hypervisor diagnostics trace"
                )
            })
            .ok();
        let capture_state = session.as_deref().and_then(|session| {
            CaptureState::start(output_dir, session)
                .inspect_err(|e| {
                    tracing::warn!(
                        error = e.as_ref() as &dyn std::error::Error,
                        "failed to start etw capture-state requests"
                    )
                })
                .ok()
        });
        Self {
            session,
            capture_state,
            output_dir: output_dir.to_owned(),
        }
    }
}

impl Drop for HostDiagnostics {
    fn drop(&mut self) {
        // Drain buffered events into the trace before stopping it.
        if let Some(capture_state) = self.capture_state.take() {
            capture_state.finish();
        }
        if let Some(session) = self.session.take() {
            match run("logman", ["stop", session.as_str(), "-ets"]) {
                Ok(_) => decode_trace(
                    &self.output_dir.join(format!("{FILE_STEM}.etl")),
                    &self.output_dir.join(format!("{FILE_STEM}.xml")),
                ),
                Err(e) => tracing::warn!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "failed to stop hypervisor diagnostics trace"
                ),
            }
        }
    }
}

/// A helper process that sends ETW capture-state requests to a session.
struct CaptureState {
    child: Child,
    stop_path: PathBuf,
}

impl CaptureState {
    fn start(output_dir: &Path, session: &str) -> anyhow::Result<Self> {
        let script_path = output_dir.join(format!("{FILE_STEM}_capture_state.ps1"));
        fs_err::write(&script_path, CAPTURE_STATE_SCRIPT)?;
        let stop_path = output_dir.join(format!("{FILE_STEM}_capture_state.stop"));
        let stderr =
            fs_err::File::create(output_dir.join(format!("{FILE_STEM}_capture_state.err")))?;
        let child = Command::new("pwsh.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .arg("-Session")
            .arg(session)
            .arg("-Provider")
            .arg(HYPERVISOR_DIAGNOSTICS_PROVIDER)
            .arg("-StopPath")
            .arg(&stop_path)
            .arg("-LogPath")
            .arg(output_dir.join(format!("{FILE_STEM}_capture_state.log")))
            .arg("-HypervisorInfoPath")
            .arg(output_dir.join("hypervisor_image.txt"))
            .arg("-ParentPid")
            .arg(std::process::id().to_string())
            .arg("-IntervalSeconds")
            .arg(CAPTURE_STATE_INTERVAL_SECS.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.into_parts().0)
            .spawn()?;
        Ok(Self { child, stop_path })
    }

    /// Requests a final capture state and waits for the helper to exit.
    fn finish(mut self) {
        if let Err(e) = fs_err::write(&self.stop_path, "") {
            tracing::warn!(
                error = &e as &dyn std::error::Error,
                "failed to signal etw capture-state helper"
            );
        }
        let deadline = Instant::now() + CAPTURE_STATE_EXIT_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                _ => break,
            }
        }
        tracing::warn!("etw capture-state helper did not exit; killing it");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run<I, S>(program: &str, args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program).args(args).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        anyhow::bail!(
            "{program} failed with {}: {}{}",
            output.status,
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(stdout)
}

fn start_trace(output_dir: &Path) -> anyhow::Result<String> {
    let (petri_sessions, total_sessions) = clean_stale_sessions()?;
    if petri_sessions >= MAX_PETRI_SESSIONS || total_sessions >= MAX_TOTAL_SESSIONS {
        anyhow::bail!(
            "too many etw sessions are running ({petri_sessions} petri, {total_sessions} total)"
        );
    }

    let provider_path = output_dir.join(format!("{FILE_STEM}_providers.txt"));
    fs_err::write(
        &provider_path,
        format!("{HYPERVISOR_DIAGNOSTICS_PROVIDER} 0xffffffffffffffff 0x5\n"),
    )?;

    let session = format!(
        "{SESSION_PREFIX}{}-{}",
        std::process::id(),
        guid::Guid::new_random()
    );
    let etl_path = output_dir.join(format!("{FILE_STEM}.etl"));
    let max_mb = MAX_TRACE_MB.to_string();
    run(
        "logman",
        [
            OsStr::new("start"),
            OsStr::new(&session),
            OsStr::new("-ets"),
            OsStr::new("-pf"),
            provider_path.as_os_str(),
            OsStr::new("-o"),
            etl_path.as_os_str(),
            OsStr::new("-bs"),
            OsStr::new("256"),
            OsStr::new("-nb"),
            OsStr::new("8"),
            OsStr::new("64"),
            OsStr::new("-max"),
            OsStr::new(&max_mb),
        ],
    )?;
    tracing::info!(session, "started hypervisor diagnostics trace");
    Ok(session)
}

/// Stops petri sessions left by processes that exited without stopping them,
/// such as tests killed on timeout. Returns the number of petri sessions and
/// of all sessions still running.
fn clean_stale_sessions() -> anyhow::Result<(usize, usize)> {
    // Querying can fail transiently when another session stops during the
    // enumeration.
    let output = run("logman", ["query", "-ets"]).or_else(|_| {
        std::thread::sleep(Duration::from_secs(1));
        run("logman", ["query", "-ets"])
    })?;
    let mut petri_sessions = 0;
    let mut total_sessions = 0;
    for line in output.lines() {
        if line.split_whitespace().last() != Some("Running") {
            continue;
        }
        total_sessions += 1;
        let Some(name) = line
            .split_whitespace()
            .next()
            .filter(|name| name.starts_with(SESSION_PREFIX))
        else {
            continue;
        };
        let pid = name[SESSION_PREFIX.len()..]
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        let alive = pid.is_none_or(|pid| pid == std::process::id() || process_exists(pid));
        if alive {
            petri_sessions += 1;
        } else if let Err(e) = run("logman", ["stop", name, "-ets"]) {
            tracing::warn!(
                session = name,
                error = e.as_ref() as &dyn std::error::Error,
                "failed to stop stale etw session"
            );
            petri_sessions += 1;
        } else {
            tracing::info!(session = name, "stopped stale etw session");
            total_sessions -= 1;
        }
    }
    Ok((petri_sessions, total_sessions))
}

fn process_exists(pid: u32) -> bool {
    // Treat lookup failures as alive so a session is never stopped while its
    // test may still be running.
    let filter = format!("PID eq {pid}");
    run("tasklist", ["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
        .map_or(true, |out| out.contains(&format!("\"{pid}\"")))
}

fn decode_trace(etl_path: &Path, xml_path: &Path) {
    let len = match fs_err::metadata(etl_path) {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::warn!(
                error = &e as &dyn std::error::Error,
                "hypervisor diagnostics trace was not written"
            );
            return;
        }
    };
    if len > MAX_DECODE_BYTES {
        tracing::warn!(
            len,
            "hypervisor diagnostics trace is too large to decode; keeping etl only"
        );
        return;
    }
    if let Err(e) = run(
        "tracerpt",
        [
            etl_path.as_os_str(),
            OsStr::new("-o"),
            xml_path.as_os_str(),
            OsStr::new("-of"),
            OsStr::new("XML"),
            // Keep events that do not match their registered schema.
            OsStr::new("-lr"),
            OsStr::new("-y"),
        ],
    ) {
        tracing::warn!(
            error = e.as_ref() as &dyn std::error::Error,
            "failed to decode hypervisor diagnostics trace"
        );
    }
}
