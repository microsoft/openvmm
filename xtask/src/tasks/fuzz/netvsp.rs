// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::cargo_fuzz::CargoFuzzCommand;
use super::parse_fuzz_crate_toml::RepoFuzzTarget;
use anyhow::Context;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

const NETVSP_TARGETS: [&str; 9] = [
    "fuzz_netvsp_control",
    "fuzz_netvsp_tx_path",
    "fuzz_netvsp_oid",
    "fuzz_netvsp_interop",
    "fuzz_netvsp_rx_path",
    "fuzz_netvsp_link_status",
    "fuzz_netvsp_vf_state",
    "fuzz_netvsp_subchannel",
    "fuzz_netvsp_save_restore",
];

pub(super) fn run_netvsp_campaign(
    fuzz_targets: BTreeMap<String, RepoFuzzTarget>,
    duration_minutes: u64,
    toolchain: Option<String>,
) -> anyhow::Result<()> {
    if duration_minutes == 0 {
        anyhow::bail!("duration_minutes must be greater than 0");
    }

    let targets = select_netvsp_targets(fuzz_targets)?;

    let first_target = targets
        .first()
        .context("internal error: netvsp target set is empty")?;
    let fuzz_dir = first_target.1.fuzz_dir.clone();

    let dict = fuzz_dir.join("netvsp_rndis.dict");
    if !dict.is_file() {
        anyhow::bail!("missing dictionary: {}", dict.display());
    }

    let log_dir = fuzz_dir.join("fuzz_logs");
    fs_err::create_dir_all(&log_dir)?;

    let max_total_time = duration_minutes.saturating_mul(60);
    println!("==============================================");
    println!(" NetVSP Fuzzing Campaign");
    println!("==============================================");
    println!(" Targets:         {}", targets.len());
    println!(
        " Duration:        {}m ({}s per target)",
        duration_minutes, max_total_time
    );
    println!(" Workers/target:  1 (single process)");
    println!(" Total workers:   {}", targets.len());
    println!(" Dictionary:      {}", dict.display());
    println!(" Sanitizers:      toolchain-dependent (ASAN on nightly/direct xtask)");
    println!(" Value profile:   enabled (CMP-guided mutation)");
    println!(" Max input len:   4096 bytes");
    println!(" Log dir:         {}", log_dir.display());
    println!(
        " Artifacts:       {}/artifacts/<target>/",
        fuzz_dir.display()
    );
    println!("==============================================");

    println!();
    println!("[*] Pre-building all {} fuzz targets...", targets.len());
    let mut build_failed = 0u32;

    for (target_name, target) in &targets {
        println!("    Building {}...", target_name);
        let extra = vec!["--".to_owned(), "-runs=0".to_owned()];

        let result = CargoFuzzCommand::Run { artifact: None }.invoke(
            target_name,
            &target.fuzz_dir,
            &target.target_options,
            toolchain.as_deref(),
            &extra,
        );

        match result {
            Ok(()) => println!("    ✓ {} built", target_name),
            Err(err) => {
                println!("    ✗ {} failed to build", target_name);
                log::error!("build failure for {}: {:#}", target_name, err);
                build_failed += 1;
            }
        }
    }

    if build_failed > 0 {
        anyhow::bail!(
            "{} / {} targets failed to build. Aborting campaign.",
            build_failed,
            targets.len()
        );
    }

    println!("[*] All targets built successfully.");

    let campaign_start = SystemTime::now();

    println!();
    println!("[*] Launching all {} targets in parallel...", targets.len());

    let mut handles = Vec::new();
    for (target_name, target) in targets {
        let toolchain = toolchain.clone();
        let fuzz_dir = target.fuzz_dir.clone();
        let target_options = target.target_options.clone();
        let dict = dict.clone();
        let target_name_for_thread = target_name.clone();

        let handle = std::thread::spawn(move || -> anyhow::Result<()> {
            let artifact_dir = fuzz_dir.join("artifacts").join(&target_name_for_thread);
            fs_err::create_dir_all(&artifact_dir)?;

            let extra = vec![
                "--".to_owned(),
                format!("-dict={}", dict.display()),
                format!("-max_total_time={}", max_total_time),
                "-timeout=10".to_owned(),
                "-use_value_profile=1".to_owned(),
                "-max_len=4096".to_owned(),
                "-report_slow_units=1".to_owned(),
                "-print_final_stats=1".to_owned(),
                format!("-artifact_prefix={}/", artifact_dir.display()),
            ];

            CargoFuzzCommand::Run { artifact: None }.invoke(
                &target_name_for_thread,
                &fuzz_dir,
                &target_options,
                toolchain.as_deref(),
                &extra,
            )
        });

        handles.push((target_name, handle));
    }

    let mut failed = 0u32;
    for (target_name, handle) in handles {
        match handle.join() {
            Ok(Ok(())) => println!("    ✓ {} completed successfully", target_name),
            Ok(Err(err)) => {
                println!("    ✗ {} failed", target_name);
                log::error!("{} failed: {:#}", target_name, err);
                failed += 1;
            }
            Err(_) => {
                println!("    ✗ {} panicked", target_name);
                failed += 1;
            }
        }
    }

    let mut total_counts = ArtifactCounts::default();
    let mut crash_timeout_files = Vec::new();

    println!();
    println!("==============================================");
    println!(" Campaign Complete");
    println!("==============================================");

    for target_name in NETVSP_TARGETS {
        let artifact_dir = fuzz_dir.join("artifacts").join(target_name);
        if !artifact_dir.exists() {
            println!("  {}: no artifacts dir", target_name);
            continue;
        }

        let (counts, files) = count_new_artifacts(&artifact_dir, campaign_start)?;
        total_counts.add(&counts);
        crash_timeout_files.extend(files);

        if counts.total() == 0 {
            println!("  {}: clean", target_name);
        } else {
            println!(
                "  {}: {} crashes, {} timeouts, {} slow-units, {} OOM",
                target_name, counts.crashes, counts.timeouts, counts.slow_units, counts.oom
            );
        }
    }

    println!();
    println!(
        " Total (new): {} crashes, {} timeouts, {} slow-units, {} OOM",
        total_counts.crashes, total_counts.timeouts, total_counts.slow_units, total_counts.oom
    );
    println!(" Targets failed: {} / {}", failed, NETVSP_TARGETS.len());

    if !crash_timeout_files.is_empty() {
        println!();
        println!("==============================================");
        println!(" New Crash/Timeout Artifacts");
        println!("==============================================");
        for file in crash_timeout_files {
            if let Ok(meta) = fs_err::metadata(&file) {
                println!("  {} ({} bytes)", file.display(), meta.len());
            } else {
                println!("  {}", file.display());
            }
        }
    }

    Ok(())
}

pub(super) fn run_netvsp_coverage(
    ctx: &crate::XtaskCtx,
    fuzz_targets: BTreeMap<String, RepoFuzzTarget>,
    toolchain: Option<String>,
    nightly: &str,
) -> anyhow::Result<()> {
    let targets = select_netvsp_targets(fuzz_targets)?;
    let first_target = targets
        .first()
        .context("internal error: netvsp target set is empty")?;
    let fuzz_dir = first_target.1.fuzz_dir.clone();

    let log_dir = fuzz_dir.join("fuzz_logs");
    let coverage_dir = fuzz_dir.join("coverage");
    let merged_dir = coverage_dir.join("merged");
    let merged_lcov = merged_dir.join("merged_netvsp.lcov");
    let text_report = merged_dir.join("coverage_report.txt");

    fs_err::create_dir_all(&log_dir)?;
    fs_err::create_dir_all(&merged_dir)?;

    ensure_coverage_prereqs(nightly)?;

    println!("==============================================");
    println!(" NetVSP Fuzz Coverage Collection");
    println!("==============================================");
    println!(" Targets:    {}", targets.len());
    println!(" Toolchain:  {}", nightly);
    println!(" Output:     {}", text_report.display());
    println!("==============================================");

    for (target_name, target) in &targets {
        println!("[*] Collecting coverage for {}...", target_name);
        if let Err(err) = CargoFuzzCommand::Coverage.invoke(
            target_name,
            &target.fuzz_dir,
            &target.target_options,
            toolchain.as_deref(),
            &[],
        ) {
            log::warn!("coverage collection failed for {}: {:#}", target_name, err);
        }
    }

    let (llvm_cov, _) = find_llvm_cov_tools(nightly)?;

    let mut filtered_files = Vec::new();
    for (target_name, _target) in &targets {
        let profdata = coverage_dir.join(target_name).join("coverage.profdata");
        if !profdata.is_file() {
            log::warn!("{}: no coverage.profdata found", target_name);
            continue;
        }

        let Some(coverage_binary) = find_coverage_binary(&ctx.root, target_name)? else {
            log::warn!("{}: no coverage binary found", target_name);
            continue;
        };

        let raw_lcov = coverage_dir.join(target_name).join("coverage.lcov");
        let filtered_tmp = coverage_dir
            .join(target_name)
            .join("coverage_netvsp.tmp.lcov");
        let filtered_lcov = coverage_dir.join(target_name).join("coverage_netvsp.lcov");

        fs_err::create_dir_all(
            raw_lcov
                .parent()
                .context("failed to resolve coverage output directory")?,
        )?;

        let export_output = Command::new(&llvm_cov)
            .arg("export")
            .arg(format!("-instr-profile={}", profdata.display()))
            .arg("-format=lcov")
            .arg("-object")
            .arg(&coverage_binary)
            .arg("--ignore-filename-regex")
            .arg("rustc")
            .arg("--ignore-filename-regex")
            .arg("openssl-sys")
            .output()
            .with_context(|| format!("failed to run {}", llvm_cov.display()))?;

        if !export_output.status.success() {
            log::warn!("{}: llvm-cov export failed", target_name);
            continue;
        }
        fs_err::write(&raw_lcov, export_output.stdout)?;

        let extract_status = Command::new("lcov")
            .arg("--extract")
            .arg(&raw_lcov)
            .arg("*/vm/devices/net/netvsp/src/*")
            .arg("--output-file")
            .arg(&filtered_tmp)
            .arg("--quiet")
            .status()
            .context("failed to invoke lcov --extract")?;
        if !extract_status.success() {
            log::warn!("{}: lcov --extract failed", target_name);
            continue;
        }

        let remove_status = Command::new("lcov")
            .arg("--remove")
            .arg(&filtered_tmp)
            .arg("*/test.rs")
            .arg("*/test_helpers.rs")
            .arg("--output-file")
            .arg(&filtered_lcov)
            .arg("--quiet")
            .status()
            .context("failed to invoke lcov --remove")?;
        let _ = fs_err::remove_file(&filtered_tmp);

        if !remove_status.success() {
            log::warn!("{}: lcov --remove failed", target_name);
            continue;
        }

        if filtered_lcov.is_file() && fs_err::metadata(&filtered_lcov)?.len() > 0 {
            filtered_files.push(filtered_lcov);
        }
    }

    if filtered_files.is_empty() {
        anyhow::bail!("no LCOV data collected; merged report not generated");
    }

    let mut merge_cmd = Command::new("lcov");
    for file in &filtered_files {
        merge_cmd.arg("--add-tracefile").arg(file);
    }
    let merge_status = merge_cmd
        .arg("--output-file")
        .arg(&merged_lcov)
        .arg("--quiet")
        .status()
        .context("failed to invoke lcov merge")?;
    if !merge_status.success() {
        anyhow::bail!("failed to merge lcov tracefiles");
    }

    write_coverage_report(&merged_lcov, &text_report)?;

    println!("[*] Text report written to: {}", text_report.display());

    Ok(())
}

fn ensure_coverage_prereqs(nightly: &str) -> anyhow::Result<()> {
    if which::which("lcov").is_err() {
        anyhow::bail!("missing prerequisite `lcov` (install via your package manager)");
    }

    let _ = find_llvm_cov_tools(nightly)?;
    Ok(())
}

/// Try the user-supplied toolchain, then probe `nightly` and `ms-nightly`.
pub(super) fn resolve_nightly_toolchain(user_toolchain: Option<&str>) -> anyhow::Result<String> {
    if let Some(tc) = user_toolchain {
        let status = Command::new("rustc")
            .arg(format!("+{}", tc))
            .arg("--print")
            .arg("sysroot")
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(tc.to_owned());
        }
        anyhow::bail!(
            "requested toolchain '{}' is not available (rustc +{} failed)",
            tc,
            tc
        );
    }

    for candidate in ["nightly", "ms-nightly"] {
        let status = Command::new("rustc")
            .arg(format!("+{}", candidate))
            .arg("--print")
            .arg("sysroot")
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(candidate.to_owned());
        }
    }

    anyhow::bail!(
        "no nightly toolchain found. \
         Install one with `rustup toolchain install nightly` or `msrustup update`."
    );
}

fn find_llvm_cov_tools(nightly: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    let sysroot = Command::new("rustc")
        .arg(format!("+{}", nightly))
        .arg("--print")
        .arg("sysroot")
        .output()
        .with_context(|| format!("failed to invoke rustc +{} --print sysroot", nightly))?;

    if !sysroot.status.success() {
        anyhow::bail!("unable to determine nightly sysroot");
    }

    let sysroot = String::from_utf8(sysroot.stdout).context("sysroot output was not utf-8")?;
    let sysroot = PathBuf::from(sysroot.trim());

    let mut profdata: Option<PathBuf> = None;
    for entry in walkdir::WalkDir::new(&sysroot)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_name() == "llvm-profdata" {
            profdata = Some(entry.path().to_path_buf());
            break;
        }
    }

    let profdata = profdata.context("llvm-profdata was not found in nightly sysroot")?;
    let llvm_cov = profdata
        .parent()
        .context("llvm-profdata had no parent")?
        .join("llvm-cov");

    if !llvm_cov.is_file() {
        anyhow::bail!("llvm-cov was not found next to llvm-profdata");
    }

    Ok((llvm_cov, profdata))
}

fn find_coverage_binary(repo_root: &Path, target_name: &str) -> anyhow::Result<Option<PathBuf>> {
    let target_root = repo_root.join("target");
    if !target_root.exists() {
        return Ok(None);
    }

    // Coverage binaries live under target/<triple>/coverage/<deps|build|...>/
    // so depth 6 is more than sufficient and avoids walking the entire target tree.
    for entry in walkdir::WalkDir::new(&target_root)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|x| x.to_str()) else {
            continue;
        };

        if file_name != target_name {
            continue;
        }

        let full_path = path.to_string_lossy();
        if full_path.contains("/coverage/") {
            return Ok(Some(path.to_path_buf()));
        }
    }

    Ok(None)
}

fn write_coverage_report(merged_lcov: &Path, text_report: &Path) -> anyhow::Result<()> {
    let summary_output = Command::new("lcov")
        .arg("--summary")
        .arg(merged_lcov)
        .output()
        .context("failed to run lcov --summary")?;

    let mut out = File::create(text_report)
        .with_context(|| format!("failed to create {}", text_report.display()))?;

    writeln!(out, "==============================================")?;
    writeln!(out, " NetVSP Fuzz Coverage Report")?;
    writeln!(out, "==============================================")?;
    writeln!(out)?;
    writeln!(
        out,
        " Source filter: netvsp/src/* (excluding test.rs, test_helpers.rs)"
    )?;
    writeln!(out, " Targets: {}", NETVSP_TARGETS.join(" "))?;
    writeln!(out)?;
    writeln!(out, "----------------------------------------------")?;
    writeln!(out, " Overall Summary")?;
    writeln!(out, "----------------------------------------------")?;

    if summary_output.status.success() {
        let summary = String::from_utf8(summary_output.stdout)
            .context("lcov --summary output was not utf-8")?;
        write!(out, "{}", summary)?;
    } else {
        writeln!(out, "lcov --summary failed")?;
    }

    writeln!(out)?;
    writeln!(out, "----------------------------------------------")?;
    writeln!(out, " Per-File Coverage")?;
    writeln!(out, "----------------------------------------------")?;
    writeln!(out)?;

    for item in parse_per_file_lcov(merged_lcov)? {
        if item.line_total == 0 {
            continue;
        }

        let line_pct = (item.line_hit as f64 / item.line_total as f64) * 100.0;
        let fn_info = if item.fn_total > 0 {
            let fn_pct = (item.fn_hit as f64 / item.fn_total as f64) * 100.0;
            format!("{}/{} fn ({:.1}%)", item.fn_hit, item.fn_total, fn_pct)
        } else {
            "-".to_owned()
        };

        writeln!(
            out,
            "  {:<40} {:>4} / {:>4} lines ({:>5.1}%)  {}",
            item.short_path, item.line_hit, item.line_total, line_pct, fn_info
        )?;
    }

    writeln!(out)?;
    writeln!(out, "----------------------------------------------")?;
    writeln!(out, " LCOV data: {}", merged_lcov.display())?;
    writeln!(out, "----------------------------------------------")?;

    Ok(())
}

#[derive(Default)]
struct ArtifactCounts {
    crashes: u32,
    timeouts: u32,
    slow_units: u32,
    oom: u32,
}

impl ArtifactCounts {
    fn add(&mut self, other: &Self) {
        self.crashes += other.crashes;
        self.timeouts += other.timeouts;
        self.slow_units += other.slow_units;
        self.oom += other.oom;
    }

    fn total(&self) -> u32 {
        self.crashes + self.timeouts + self.slow_units + self.oom
    }
}

fn count_new_artifacts(
    artifact_dir: &Path,
    campaign_start: SystemTime,
) -> anyhow::Result<(ArtifactCounts, Vec<PathBuf>)> {
    let mut counts = ArtifactCounts::default();
    let mut crash_timeout_files = Vec::new();

    for entry in walkdir::WalkDir::new(artifact_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let modified = fs_err::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if modified <= campaign_start {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|x| x.to_str()) else {
            continue;
        };

        if file_name.starts_with("crash-") {
            counts.crashes += 1;
            crash_timeout_files.push(path.to_path_buf());
        } else if file_name.starts_with("timeout-") {
            counts.timeouts += 1;
            crash_timeout_files.push(path.to_path_buf());
        } else if file_name.starts_with("slow-unit-") {
            counts.slow_units += 1;
        } else if file_name.starts_with("oom-") {
            counts.oom += 1;
        }
    }

    Ok((counts, crash_timeout_files))
}

fn select_netvsp_targets(
    mut fuzz_targets: BTreeMap<String, RepoFuzzTarget>,
) -> anyhow::Result<Vec<(String, RepoFuzzTarget)>> {
    let mut targets = Vec::new();

    for name in NETVSP_TARGETS {
        let target = fuzz_targets
            .remove(name)
            .with_context(|| format!("missing expected NetVSP fuzz target: {name}"))?;
        targets.push((name.to_owned(), target));
    }

    Ok(targets)
}

struct LcovFileCoverage {
    short_path: String,
    line_total: u64,
    line_hit: u64,
    fn_total: u64,
    fn_hit: u64,
}

fn parse_per_file_lcov(path: &Path) -> anyhow::Result<Vec<LcovFileCoverage>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut result = Vec::new();

    let mut current_file: Option<String> = None;
    let mut line_total = 0u64;
    let mut line_hit = 0u64;
    let mut fn_total = 0u64;
    let mut fn_hit = 0u64;

    for line in reader.lines() {
        let line = line?;

        if let Some(value) = line.strip_prefix("SF:") {
            current_file = Some(value.to_owned());
            line_total = 0;
            line_hit = 0;
            fn_total = 0;
            fn_hit = 0;
            continue;
        }

        if let Some(value) = line.strip_prefix("LF:") {
            line_total = value.parse().unwrap_or(0);
            continue;
        }

        if let Some(value) = line.strip_prefix("LH:") {
            line_hit = value.parse().unwrap_or(0);
            continue;
        }

        if let Some(value) = line.strip_prefix("FNF:") {
            fn_total = value.parse().unwrap_or(0);
            continue;
        }

        if let Some(value) = line.strip_prefix("FNH:") {
            fn_hit = value.parse().unwrap_or(0);
            continue;
        }

        if line == "end_of_record" {
            if let Some(file) = current_file.take() {
                let short_path = if let Some((_, suffix)) = file.rsplit_once("/netvsp/src/") {
                    suffix.to_owned()
                } else {
                    file
                };

                result.push(LcovFileCoverage {
                    short_path,
                    line_total,
                    line_hit,
                    fn_total,
                    fn_hit,
                });
            }
        }
    }

    Ok(result)
}
