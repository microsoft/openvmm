// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measures the host-side snapshot foundations used by microVM phase 2.

use anyhow::Context;
use openvmm_helpers::snapshot::MANIFEST_VERSION;
use openvmm_helpers::snapshot::SnapshotManifest;
use openvmm_helpers::snapshot::publish::write_snapshot;
use openvmm_helpers::snapshot::restore::read_snapshot_with_memory;
use sparse_mmap::SparseMapping;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;

const RESULT_PREFIX: &str = "OPENVMM_PHASE2_RESULT=";
const STATE_SIZE_BYTES: usize = 64 * 1024;

struct Options {
    warmups: usize,
    runs: usize,
    memory_mib: usize,
    restore_worker: Option<PathBuf>,
}

impl Options {
    fn parse() -> anyhow::Result<Self> {
        let mut options = Self {
            warmups: 3,
            runs: 11,
            memory_mib: 128,
            restore_worker: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--restore-worker" => {
                    options.restore_worker = Some(PathBuf::from(
                        args.next().context("missing value for --restore-worker")?,
                    ));
                    continue;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: phase2_snapshot_bench [--warmups N] [--runs N] \
                         [--memory-mib N]"
                    );
                    std::process::exit(0);
                }
                "--warmups" | "--runs" | "--memory-mib" => {}
                _ => anyhow::bail!("unknown argument: {arg}"),
            }
            let value = args
                .next()
                .with_context(|| format!("missing value for {arg}"))?
                .parse::<usize>()
                .with_context(|| format!("invalid value for {arg}"))?;
            anyhow::ensure!(value > 0, "{arg} must be greater than zero");
            match arg.as_str() {
                "--warmups" => options.warmups = value,
                "--runs" => options.runs = value,
                "--memory-mib" => options.memory_mib = value,
                _ => unreachable!(),
            }
        }
        Ok(options)
    }
}

#[derive(Default)]
struct Samples {
    snapshot_publish_ms: Vec<f64>,
    restore_prepare_ms: Vec<f64>,
    snapshot_verify_ms: Vec<f64>,
    cow_map_ms: Vec<f64>,
    cow_dirty_all_ms: Vec<f64>,
    repeat_restore_prepare_ms: Vec<f64>,
    new_process_restore_prepare_ms: Vec<f64>,
    repeat_verify_ms: Vec<f64>,
    repeat_cow_map_ms: Vec<f64>,
}

struct Iteration {
    snapshot_publish_ms: f64,
    restore_prepare_ms: f64,
    snapshot_verify_ms: f64,
    cow_map_ms: f64,
    cow_dirty_all_ms: f64,
    repeat_restore_prepare_ms: f64,
    new_process_restore_prepare_ms: f64,
    repeat_verify_ms: f64,
    repeat_cow_map_ms: f64,
}

impl Samples {
    fn push(&mut self, sample: Iteration) {
        self.snapshot_publish_ms.push(sample.snapshot_publish_ms);
        self.restore_prepare_ms.push(sample.restore_prepare_ms);
        self.snapshot_verify_ms.push(sample.snapshot_verify_ms);
        self.cow_map_ms.push(sample.cow_map_ms);
        self.cow_dirty_all_ms.push(sample.cow_dirty_all_ms);
        self.repeat_restore_prepare_ms
            .push(sample.repeat_restore_prepare_ms);
        self.new_process_restore_prepare_ms
            .push(sample.new_process_restore_prepare_ms);
        self.repeat_verify_ms.push(sample.repeat_verify_ms);
        self.repeat_cow_map_ms.push(sample.repeat_cow_map_ms);
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn create_memory_source(path: &Path, size: u64) -> anyhow::Result<()> {
    let mut file = fs_err::File::create(path).context("failed to create memory source")?;
    file.set_len(size).context("failed to size memory source")?;
    const INITIALIZE_CHUNK_BYTES: usize = 1024 * 1024;
    let chunk = vec![0x6d_u8; INITIALIZE_CHUNK_BYTES];
    let mut remaining = size;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(INITIALIZE_CHUNK_BYTES as u64)).unwrap();
        file.write_all(&chunk[..count])
            .context("failed to initialize memory source")?;
        remaining -= count as u64;
    }
    file.seek(SeekFrom::Start(0))
        .context("failed to seek in memory source")?;
    file.write_all(&[0x5a])
        .context("failed to seed memory source")?;
    file.seek(SeekFrom::End(-1))
        .context("failed to seek in memory source")?;
    file.write_all(&[0xa5])
        .context("failed to seed end of memory source")?;
    file.sync_all().context("failed to flush memory source")?;
    Ok(())
}

fn map_copy_on_write(
    memory_file: std::fs::File,
    memory_size: usize,
) -> anyhow::Result<SparseMapping> {
    let mappable = sparse_mmap::new_mappable_from_file_copy_on_write(&memory_file, false)
        .context("failed to create copy-on-write memory handle")?;
    let mapping = SparseMapping::new(memory_size).context("failed to reserve memory mapping")?;
    mapping
        .map_file_copy_on_write(0, memory_size, &mappable, 0, true)
        .context("failed to map snapshot memory copy-on-write")?;
    Ok(mapping)
}

fn validate_original(mapping: &SparseMapping, memory_size: usize) -> anyhow::Result<()> {
    let mut byte = [0_u8; 1];
    mapping.read_at(0, &mut byte)?;
    anyhow::ensure!(byte == [0x5a], "snapshot first byte changed");
    mapping.read_at(memory_size - 1, &mut byte)?;
    anyhow::ensure!(byte == [0xa5], "snapshot last byte changed");
    Ok(())
}

fn run_iteration(
    root: &Path,
    iteration: usize,
    executable: &Path,
    memory_path: &Path,
    memory_size: usize,
    state: &[u8],
) -> anyhow::Result<Iteration> {
    let snapshot_path = root.join(format!("snapshot-{iteration}"));
    let manifest = SnapshotManifest {
        version: MANIFEST_VERSION,
        created_at: std::time::SystemTime::now().into(),
        openvmm_version: env!("CARGO_PKG_VERSION").to_owned(),
        memory_size_bytes: memory_size as u64,
        vp_count: 1,
        page_size: SparseMapping::page_size() as u32,
        architecture: "x86_64".to_owned(),
        state_size_bytes: 0,
        state_sha256: Vec::new(),
        memory_sha256: Vec::new(),
        machine_contract: None,
        format_magic: openvmm_helpers::snapshot::format::SNAPSHOT_FORMAT_MAGIC.to_vec(),
        saved_state_schema_version: openvmm_helpers::snapshot::format::SAVED_STATE_SCHEMA_VERSION,
        saved_state_root_type: openvmm_helpers::snapshot::format::SAVED_STATE_ROOT_TYPE.to_owned(),
        snapshot_tier: String::new(),
        restore_policy: String::new(),
        consumed_config_sections: 0,
    };

    let started = Instant::now();
    write_snapshot(&snapshot_path, &manifest, state, memory_path)?;
    let snapshot_publish_ms = elapsed_ms(started);

    let restore_started = Instant::now();
    let started = Instant::now();
    let (_, restored_state, memory_file) =
        read_snapshot_with_memory(&snapshot_path, memory_size as u64)?;
    let snapshot_verify_ms = elapsed_ms(started);
    anyhow::ensure!(restored_state == state, "saved state changed");

    let started = Instant::now();
    let mapping = map_copy_on_write(memory_file, memory_size)?;
    let cow_map_ms = elapsed_ms(started);
    validate_original(&mapping, memory_size)?;
    let restore_prepare_ms = elapsed_ms(restore_started);

    let started = Instant::now();
    mapping.fill_at(0, 0x3c, memory_size)?;
    let cow_dirty_all_ms = elapsed_ms(started);
    drop(mapping);

    let repeat_restore_started = Instant::now();
    let started = Instant::now();
    let (_, restored_state, memory_file) =
        read_snapshot_with_memory(&snapshot_path, memory_size as u64)?;
    let repeat_verify_ms = elapsed_ms(started);
    anyhow::ensure!(
        restored_state == state,
        "saved state changed after COW dirtying"
    );

    let started = Instant::now();
    let mapping = map_copy_on_write(memory_file, memory_size)?;
    let repeat_cow_map_ms = elapsed_ms(started);
    validate_original(&mapping, memory_size)?;
    let repeat_restore_prepare_ms = elapsed_ms(repeat_restore_started);
    drop(mapping);

    let started = Instant::now();
    let status = Command::new(executable)
        .arg("--restore-worker")
        .arg(&snapshot_path)
        .arg("--memory-mib")
        .arg((memory_size / (1024 * 1024)).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .context("failed to launch restore-preparation worker")?;
    anyhow::ensure!(status.success(), "restore-preparation worker failed");
    let new_process_restore_prepare_ms = elapsed_ms(started);

    fs_err::remove_dir_all(&snapshot_path).context("failed to remove benchmark snapshot")?;
    Ok(Iteration {
        snapshot_publish_ms,
        restore_prepare_ms,
        snapshot_verify_ms,
        cow_map_ms,
        cow_dirty_all_ms,
        repeat_restore_prepare_ms,
        new_process_restore_prepare_ms,
        repeat_verify_ms,
        repeat_cow_map_ms,
    })
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn nearest_rank_percentile(samples: &[f64], percentile: usize) -> f64 {
    assert!(!samples.is_empty());
    assert!((1..=100).contains(&percentile));
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = (percentile * sorted.len()).div_ceil(100) - 1;
    sorted[index]
}

fn metric_json(name: &str, samples: &[f64]) -> String {
    let values = samples
        .iter()
        .map(|sample| format!("{sample:.6}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "\"{name}\":{{\"samples_ms\":[{values}],\"p50_ms\":{:.6},\
         \"p95_ms\":{:.6},\"min_ms\":{:.6},\"max_ms\":{:.6}}}",
        median(samples),
        nearest_rank_percentile(samples, 95),
        samples.iter().copied().fold(f64::INFINITY, f64::min),
        samples.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    )
}

fn print_result(options: &Options, samples: &Samples) {
    let metrics = [
        metric_json("snapshot_publish", &samples.snapshot_publish_ms),
        metric_json("restore_prepare", &samples.restore_prepare_ms),
        metric_json("snapshot_verify", &samples.snapshot_verify_ms),
        metric_json("cow_map", &samples.cow_map_ms),
        metric_json("cow_dirty_all", &samples.cow_dirty_all_ms),
        metric_json("repeat_restore_prepare", &samples.repeat_restore_prepare_ms),
        metric_json(
            "new_process_restore_prepare",
            &samples.new_process_restore_prepare_ms,
        ),
        metric_json("repeat_verify", &samples.repeat_verify_ms),
        metric_json("repeat_cow_map", &samples.repeat_cow_map_ms),
    ];
    println!(
        "{RESULT_PREFIX}{{\"memory_mib\":{},\"state_bytes\":{},\
         \"artifact_unchanged\":true,\"metrics\":{{{}}}}}",
        options.memory_mib,
        STATE_SIZE_BYTES,
        metrics.join(",")
    );
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse()?;
    let memory_size = options
        .memory_mib
        .checked_mul(1024 * 1024)
        .context("memory size overflow")?;
    anyhow::ensure!(
        memory_size >= SparseMapping::page_size(),
        "memory size must be at least one host page"
    );

    if let Some(snapshot_path) = &options.restore_worker {
        let (_, _, memory_file) = read_snapshot_with_memory(snapshot_path, memory_size as u64)?;
        let mapping = map_copy_on_write(memory_file, memory_size)?;
        validate_original(&mapping, memory_size)?;
        mapping.write_at(0, &[0x3c])?;
        return Ok(());
    }

    let root = tempfile::tempdir().context("failed to create benchmark directory")?;
    let executable = std::env::current_exe().context("failed to locate benchmark executable")?;
    let memory_path = root.path().join("memory-source.bin");
    create_memory_source(&memory_path, memory_size as u64)?;
    let state = vec![0xc3_u8; STATE_SIZE_BYTES];

    for index in 0..options.warmups {
        let sample = run_iteration(
            root.path(),
            index,
            &executable,
            &memory_path,
            memory_size,
            &state,
        )?;
        println!(
            "warmup {}/{}: publish={:.3} ms restore-prep={:.3} ms COW-dirty={:.3} ms",
            index + 1,
            options.warmups,
            sample.snapshot_publish_ms,
            sample.restore_prepare_ms,
            sample.cow_dirty_all_ms,
        );
    }

    let mut samples = Samples::default();
    for index in 0..options.runs {
        let sample = run_iteration(
            root.path(),
            options.warmups + index,
            &executable,
            &memory_path,
            memory_size,
            &state,
        )?;
        println!(
            "sample {}/{}: publish={:.3} ms restore-prep={:.3} ms verify={:.3} ms \
             map={:.3} ms dirty={:.3} ms repeat-restore-prep={:.3} ms \
             new-process-restore-prep={:.3} ms",
            index + 1,
            options.runs,
            sample.snapshot_publish_ms,
            sample.restore_prepare_ms,
            sample.snapshot_verify_ms,
            sample.cow_map_ms,
            sample.cow_dirty_all_ms,
            sample.repeat_restore_prepare_ms,
            sample.new_process_restore_prepare_ms,
        );
        samples.push(sample);
    }
    print_result(&options, &samples);
    Ok(())
}
