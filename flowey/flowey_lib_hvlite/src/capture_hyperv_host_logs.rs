// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Capture unfiltered Hyper-V host logs for the duration of a VMM test run.
//!
//! On start, this enables the Hyper-V worker and management analytic event log
//! channels and starts an ETW session for the Hyper-V providers that run in the
//! VM worker process (`vmwp.exe`) and its management services, plus process
//! start and exit events. On stop, it stops the session, exports every Hyper-V
//! event log channel for the run without filtering by VM, and restores the
//! original analytic channel configuration.
//!
//! Output is written to `hyperv_host_logs` in the test output directory so it
//! is published with the test logs. Collection failures are recorded in
//! `status.log` there and never fail the job. If the job is cancelled before
//! the stop step runs, the session and channels stay enabled until the next
//! start step on the same host.
//!
//! **Note:** This node only supports Windows.

use flowey::node::prelude::*;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;

flowey_request! {
    pub enum Request {
        /// Start capturing, writing into the given test output directory.
        Start {
            output_dir: ReadVar<PathBuf>,
            done: WriteVar<SideEffect>,
        },
        /// Stop capturing and export logs into the given test output directory.
        Stop {
            output_dir: ReadVar<PathBuf>,
            after_tests: ReadVar<SideEffect>,
            done: WriteVar<SideEffect>,
        },
    }
}

const DIR: &str = "hyperv_host_logs";
const SESSION: &str = "openvmm-ci-hyperv-host";

/// Analytic channels enabled for the run. Admin and operational channels are
/// always enabled.
const ANALYTIC_CHANNELS: &[&str] = &[
    "Microsoft-Windows-Hyper-V-Worker-Analytic",
    "Microsoft-Windows-Hyper-V-Worker-VDev-Analytic",
    "Microsoft-Windows-Hyper-V-VMMS-Analytic",
    "Microsoft-Windows-Hyper-V-Config-Analytic",
    "Microsoft-Windows-Hyper-V-Compute-Analytic",
    "Microsoft-Windows-Hyper-V-VID-Analytic",
];
const ANALYTIC_CHANNEL_MAX_BYTES: u64 = 1 << 30;

/// Hyper-V providers hosted by the VM worker process and its management
/// services. Per-packet and per-I/O kernel providers are excluded to avoid
/// perturbing test timing.
const PROVIDERS: &[&str] = &[
    "Microsoft-Windows-Hyper-V-Worker",
    "Microsoft-Windows-Hyper-V-Chipset",
    "Microsoft-Windows-Hyper-V-Config",
    "Microsoft-Windows-Hyper-V-VMMS",
    "Microsoft-Windows-Hyper-V-Compute",
    "Microsoft-Windows-Hyper-V-ComputeLib",
    "Microsoft-Windows-Hyper-V-VID",
    "Microsoft-Windows-Hyper-V-DynMem",
    "Microsoft-Windows-Hyper-V-EmulatedDevices",
    "Microsoft-Windows-Hyper-V-EmulatedNic",
    "Microsoft-Windows-Hyper-V-EmulatedStor",
    "Microsoft-Windows-Hyper-V-Integration",
    "Microsoft-Windows-Hyper-V-Serial",
    "Microsoft-Windows-Hyper-V-SynthFcVdev",
    "Microsoft-Windows-Hyper-V-SynthNic",
    "Microsoft-Windows-Hyper-V-SynthStor",
    "Microsoft-Windows-Hyper-V-Tpm",
    "Microsoft-Windows-Hyper-V-UiDevices",
    "Microsoft-Windows-Hyper-V-VmbusVdev",
    "Microsoft-Windows-Hyper-V-Virtual-PMEM",
    "Microsoft-Windows-Hyper-V-CrashDump",
    "Microsoft.Windows.HyperV.VmIcCore",
    "Microsoft.Windows.HyperV.GpupVDev",
];

/// `Microsoft-Windows-Kernel-Process` with `WINEVENT_KEYWORD_PROCESS`, for
/// worker process start and exit.
const KERNEL_PROCESS: &str = "{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716} 0x10 0x4";

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if !matches!(ctx.platform(), FlowPlatform::Windows) {
            anyhow::bail!(
                "capture_hyperv_host_logs only supports Windows. \
                Callers should check the platform before requesting this node."
            );
        }

        match request {
            Request::Start { output_dir, done } => {
                ctx.emit_rust_step("start capturing hyper-v host logs", |ctx| {
                    let output_dir = output_dir.claim(ctx);
                    done.claim(ctx);
                    move |rt| {
                        Log::run(&rt.read(output_dir).join(DIR), start);
                        Ok(())
                    }
                });
            }
            Request::Stop {
                output_dir,
                after_tests,
                done,
            } => {
                ctx.emit_rust_step("stop capturing and export hyper-v host logs", |ctx| {
                    let output_dir = output_dir.claim(ctx);
                    after_tests.claim(ctx);
                    done.claim(ctx);
                    move |rt| {
                        Log::run(&rt.read(output_dir).join(DIR), stop);
                        Ok(())
                    }
                });
            }
        }

        Ok(())
    }
}

/// Records collection progress in `status.log` and the job log.
struct Log(String);

impl Log {
    fn run(dir: &Path, f: fn(&Path, &mut Log) -> anyhow::Result<()>) {
        let mut log = Log(String::new());
        let result = fs_err::create_dir_all(dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| f(dir, &mut log));
        if let Err(e) = result {
            log.line(format!("failed: {e:#}"));
        }
        let path = dir.join("status.log");
        let existing = fs_err::read_to_string(&path).unwrap_or_default();
        if let Err(e) = fs_err::write(&path, existing + &log.0) {
            log::warn!("failed to write {}: {e}", path.display());
        }
    }

    fn line(&mut self, s: impl AsRef<str>) {
        let s = s.as_ref();
        log::info!("{s}");
        let _ = writeln!(self.0, "{s}");
    }

    /// Runs a command, recording its output. Returns stdout on success.
    fn cmd(&mut self, program: &str, args: &[&str]) -> Option<String> {
        self.line(format!("$ {program} {}", args.join(" ")));
        match Command::new(program).args(args).output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr);
                for l in stdout
                    .lines()
                    .chain(stderr.lines())
                    .filter(|l| !l.trim().is_empty())
                {
                    self.line(format!("  {}", l.trim_end()));
                }
                if out.status.success() {
                    Some(stdout)
                } else {
                    self.line(format!("  exited with {}", out.status));
                    None
                }
            }
            Err(e) => {
                self.line(format!("  failed to run: {e}"));
                None
            }
        }
    }
}

fn start(dir: &Path, log: &mut Log) -> anyhow::Result<()> {
    let start = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    fs_err::write(dir.join("start_time"), start.as_secs().to_string())?;

    // Stop a session left by a job that was cancelled before stopping it.
    if log.cmd("logman", &["query", SESSION, "-ets"]).is_some() {
        log.cmd("logman", &["stop", SESSION, "-ets"]);
    }

    let mut original = String::new();
    for channel in ANALYTIC_CHANNELS {
        let Some(config) = log.cmd("wevtutil", &["gl", channel]) else {
            continue;
        };
        let enabled = config_value(&config, "enabled").unwrap_or("false");
        let max_size = config_value(&config, "maxSize").unwrap_or("1052672");
        writeln!(original, "{channel} {enabled} {max_size}")?;
        log.cmd("wevtutil", &["sl", channel, "/e:false", "/q:true"]);
        log.cmd(
            "wevtutil",
            &["sl", channel, &format!("/ms:{ANALYTIC_CHANNEL_MAX_BYTES}")],
        );
        log.cmd("wevtutil", &["sl", channel, "/e:true", "/q:true"]);
        log.cmd("wevtutil", &["gl", channel]);
    }
    fs_err::write(dir.join("original_channel_config"), original)?;

    let registered = log
        .cmd("logman", &["query", "providers"])
        .unwrap_or_default();
    let mut provider_file = String::new();
    for name in PROVIDERS {
        match registered.lines().find_map(|line| {
            let line = line.trim();
            let brace = line.rfind('{')?;
            (line.ends_with('}') && line[..brace].trim().eq_ignore_ascii_case(name))
                .then(|| &line[brace..])
        }) {
            Some(guid) => writeln!(provider_file, "{guid} 0xffffffffffffffff 0x5")?,
            None => log.line(format!("provider {name} is not registered")),
        }
    }
    writeln!(provider_file, "{KERNEL_PROCESS}")?;
    let provider_path = dir.join("providers.txt");
    fs_err::write(&provider_path, &provider_file)?;
    log.line(format!("providers:\n{provider_file}"));

    let etl = dir.join("hyperv_host.etl");
    log.cmd(
        "logman",
        &[
            "start",
            SESSION,
            "-ets",
            "-pf",
            &provider_path.to_string_lossy(),
            "-o",
            &etl.to_string_lossy(),
            "-bs",
            "1024",
            "-nb",
            "32",
            "128",
            "-max",
            "4096",
        ],
    );
    log.cmd("logman", &["query", SESSION, "-ets"]);
    Ok(())
}

fn stop(dir: &Path, log: &mut Log) -> anyhow::Result<()> {
    log.cmd("logman", &["query", SESSION, "-ets"]);
    log.cmd("logman", &["stop", SESSION, "-ets"]);

    // Analytic channels must be disabled before they can be read.
    let original = fs_err::read_to_string(dir.join("original_channel_config")).unwrap_or_default();
    for channel in ANALYTIC_CHANNELS {
        log.cmd("wevtutil", &["gl", channel]);
        log.cmd("wevtutil", &["sl", channel, "/e:false", "/q:true"]);
    }

    // Export every Hyper-V channel since shortly before the run started,
    // without filtering by VM.
    let start: u64 = fs_err::read_to_string(dir.join("start_time"))?
        .trim()
        .parse()?;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let window = now.saturating_sub(Duration::from_secs(start)) + Duration::from_secs(300);
    let query = format!(
        "*[System[TimeCreated[timediff(@SystemTime) <= {}]]]",
        window.as_millis()
    );
    let channel_dir = dir.join("channels");
    fs_err::create_dir_all(&channel_dir)?;
    let channels = log.cmd("wevtutil", &["el"]).unwrap_or_default();
    for channel in channels
        .lines()
        .map(str::trim)
        .filter(|c| c.starts_with("Microsoft-Windows-Hyper-V-") && !c.contains("Guest-Drivers"))
    {
        let name = channel.replace(['/', '\\', ':'], "_");
        let evtx = channel_dir.join(format!("{name}.evtx"));
        let xml = channel_dir.join(format!("{name}.xml"));
        log.cmd(
            "wevtutil",
            &[
                "epl",
                channel,
                &evtx.to_string_lossy(),
                &format!("/q:{query}"),
                "/ow:true",
            ],
        );
        // Rendered XML keeps the structured event data (including VmId),
        // process and thread IDs, activity IDs, and rendered messages.
        match Command::new("wevtutil")
            .args([
                "qe",
                channel,
                &format!("/q:{query}"),
                "/f:RenderedXml",
                "/e:Events",
            ])
            .stdout(Stdio::from(fs_err::File::create(&xml)?.into_parts().0))
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => log.line(format!("rendering {channel} exited with {status}")),
            Err(e) => log.line(format!("failed to render {channel}: {e}")),
        }
        let size = fs_err::metadata(&xml).map_or(0, |m| m.len());
        log.line(format!("{channel}: {size} bytes of rendered events"));
    }

    // Restore the original analytic channel configuration.
    for line in original.lines() {
        let mut parts = line.split_whitespace();
        let (Some(channel), Some(enabled), Some(max_size)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        log.cmd("wevtutil", &["sl", channel, &format!("/ms:{max_size}")]);
        if enabled == "true" {
            log.cmd("wevtutil", &["sl", channel, "/e:true", "/q:true"]);
        }
        log.cmd("wevtutil", &["gl", channel]);
    }
    Ok(())
}

/// Returns the value of `key` from `wevtutil gl` output.
fn config_value<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    config.lines().find_map(|line| {
        let (k, v) = line.trim().split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}
