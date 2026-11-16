// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-side diagnostics for Hyper-V VMs that can fail before OpenHCL
//! produces any output.
//!
//! For the lifetime of the VM this records the Hyper-V ETW providers involved
//! in creating and starting a VM, and on drop it decodes the trace and exports
//! the Hyper-V admin and operational event log channels for the same period
//! into the test output directory.
//!
//! The trace is a host-wide ETW session, so it also contains events for any
//! other VMs running concurrently on the host.

use jiff::SignedDuration;
use jiff::Timestamp;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Hyper-V providers enabled by exact name.
const PROVIDERS: &[&str] = &[
    "Microsoft-Windows-Hyper-V-Worker",
    "Microsoft-Windows-Hyper-V-Chipset",
    "Microsoft-Windows-Hyper-V-VMMS",
    "Microsoft-Windows-Hyper-V-Hypervisor",
    "Microsoft-Windows-Hyper-V-VID",
    "Microsoft-Windows-Hyper-V-Compute",
    "Microsoft-Windows-Hyper-V-Config",
];

/// Registered Hyper-V providers whose lowercase names contain any of these are
/// also enabled.
const PROVIDER_NAME_KEYWORDS: &[&str] = &["vtl", "igvm", "hcl", "isolat", "paravisor"];

const SESSION_PREFIX: &str = "petri-hv-";
/// Stay well below the system-wide limit of 64 ETW sessions.
const MAX_PETRI_SESSIONS: usize = 24;
const MAX_TRACE_MB: u32 = 256;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

/// Records Hyper-V host diagnostics until dropped.
pub(super) struct HostDiagnostics {
    session: Option<String>,
    etl_path: PathBuf,
    output_dir: PathBuf,
    start_time: Timestamp,
}

impl HostDiagnostics {
    /// Starts collecting diagnostics into `output_dir`.
    ///
    /// Failures are logged and leave collection disabled; they never fail the
    /// test.
    pub fn start(output_dir: &Path) -> Self {
        let start_time = Timestamp::now();
        let etl_path = output_dir.join("hyperv_etw.etl");
        let session = start_trace(output_dir, &etl_path)
            .inspect_err(|e| {
                tracing::warn!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "failed to start hyper-v etw trace"
                )
            })
            .ok();
        Self {
            session,
            etl_path,
            output_dir: output_dir.to_owned(),
            start_time,
        }
    }
}

impl Drop for HostDiagnostics {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            match run("logman", ["stop", session.as_str(), "-ets"]) {
                Ok(_) => decode_trace(&self.etl_path, &self.output_dir),
                Err(e) => tracing::warn!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "failed to stop hyper-v etw trace"
                ),
            }
        }
        if let Err(e) = export_channels(&self.output_dir, self.start_time) {
            tracing::warn!(
                error = e.as_ref() as &dyn std::error::Error,
                "failed to export hyper-v event log channels"
            );
        }
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

fn start_trace(output_dir: &Path, etl_path: &Path) -> anyhow::Result<String> {
    let active = clean_stale_sessions()?;
    if active >= MAX_PETRI_SESSIONS {
        anyhow::bail!("{active} petri etw sessions are already running");
    }

    let providers = hyperv_provider_guids()?;
    if providers.is_empty() {
        anyhow::bail!("no hyper-v etw providers are registered");
    }
    let mut provider_file = String::new();
    for (name, guid) in &providers {
        tracing::debug!(name, guid, "enabling hyper-v etw provider");
        writeln!(provider_file, "{guid} 0xffffffffffffffff 0x5")?;
    }
    let provider_path = output_dir.join("hyperv_etw_providers.txt");
    fs_err::write(&provider_path, provider_file)?;

    let session = format!(
        "{SESSION_PREFIX}{}-{}",
        std::process::id(),
        guid::Guid::new_random()
    );
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
    tracing::info!(
        session,
        providers = providers.len(),
        "started hyper-v etw trace"
    );
    Ok(session)
}

/// Returns the registered Hyper-V providers to enable, as (name, GUID).
fn hyperv_provider_guids() -> anyhow::Result<Vec<(String, String)>> {
    let output = run("logman", ["query", "providers"])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let brace = line.rfind('{')?;
            if !line.ends_with('}') {
                return None;
            }
            let name = line[..brace].trim();
            let lower = name.to_ascii_lowercase();
            let wanted = PROVIDERS.iter().any(|p| p.eq_ignore_ascii_case(name))
                || (lower.starts_with("microsoft-windows-hyper-v-")
                    && PROVIDER_NAME_KEYWORDS.iter().any(|k| lower.contains(k)));
            wanted.then(|| (name.to_owned(), line[brace..].to_owned()))
        })
        .collect())
}

/// Stops petri sessions left by processes that exited without stopping them,
/// such as tests killed on timeout. Returns the number still running.
fn clean_stale_sessions() -> anyhow::Result<usize> {
    let output = run("logman", ["query", "-ets"])?;
    let mut active = 0;
    for name in output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.starts_with(SESSION_PREFIX))
    {
        let pid = name[SESSION_PREFIX.len()..]
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok());
        let alive = pid.is_none_or(|pid| pid == std::process::id() || process_exists(pid));
        if alive {
            active += 1;
        } else if let Err(e) = run("logman", ["stop", name, "-ets"]) {
            tracing::warn!(
                session = name,
                error = e.as_ref() as &dyn std::error::Error,
                "failed to stop stale etw session"
            );
            active += 1;
        } else {
            tracing::info!(session = name, "stopped stale etw session");
        }
    }
    Ok(active)
}

fn process_exists(pid: u32) -> bool {
    // Treat lookup failures as alive so a session is never stopped while its
    // test may still be running.
    let filter = format!("PID eq {pid}");
    run("tasklist", ["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
        .map_or(true, |out| out.contains(&format!("\"{pid}\"")))
}

fn decode_trace(etl_path: &Path, output_dir: &Path) {
    let len = match fs_err::metadata(etl_path) {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::warn!(
                error = &e as &dyn std::error::Error,
                "hyper-v etw trace was not written"
            );
            return;
        }
    };
    if len > MAX_DECODE_BYTES {
        tracing::warn!(
            len,
            "hyper-v etw trace is too large to decode; keeping etl only"
        );
        return;
    }
    let xml_path = output_dir.join("hyperv_etw.xml");
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
            "failed to decode hyper-v etw trace"
        );
    }
}

/// Exports the Hyper-V admin and operational channels for the period since
/// `start_time`, including events from other VMs on the host.
fn export_channels(output_dir: &Path, start_time: Timestamp) -> anyhow::Result<()> {
    let since = start_time
        .checked_sub(SignedDuration::from_secs(5))?
        .strftime("%Y-%m-%dT%H:%M:%SZ");
    let query = format!("/q:*[System[TimeCreated[@SystemTime>='{since}']]]");
    let mut out = String::new();
    for channel in run("wevtutil", ["el"])?
        .lines()
        .map(str::trim)
        .filter(|c| c.starts_with("Microsoft-Windows-Hyper-V-"))
        .filter(|c| c.ends_with("/Admin") || c.ends_with("/Operational"))
    {
        match run("wevtutil", ["qe", channel, query.as_str(), "/f:text"]) {
            Ok(events) if events.trim().is_empty() => {}
            Ok(events) => writeln!(out, "===== {channel}\n{events}")?,
            Err(e) => writeln!(out, "===== {channel}\nfailed to query: {e:#}\n")?,
        }
    }
    fs_err::write(output_dir.join("hyperv_channels.log"), out)?;
    Ok(())
}
