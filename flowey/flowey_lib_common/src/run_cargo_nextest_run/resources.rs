// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-wide measurements, deliberately independent of test process lifetimes:
//! VM memory and hypervisor work need not belong to nextest's process tree.

use crate::gen_cargo_nextest_run_cmd::Script;
use flowey::node::prelude::*;
use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use sysinfo::DiskRefreshKind;
use sysinfo::Disks;
use sysinfo::System;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct ResourceMonitor {
    dir: PathBuf,
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

impl ResourceMonitor {
    pub(super) fn start(
        config_file: &Path,
        profile: &str,
        command: &Script,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            sysinfo::IS_SUPPORTED_SYSTEM,
            "sysinfo does not support this host"
        );
        let tempdir = tempfile::Builder::new()
            .prefix("nextest-resources-")
            .tempdir_in(std::env::current_dir()?)?;
        let dir = tempdir.path();
        fs_err::copy(config_file, dir.join("nextest.toml"))?;

        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        let disks =
            Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_io_usage());
        let started = Instant::now();
        let started_unix_seconds = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs_f64();
        let scheduler_env = ["NEXTEST_TEST_THREADS", "NEXTEST_RETRIES"]
            .into_iter()
            .filter_map(|key| {
                command
                    .env
                    .get(key)
                    .cloned()
                    .or_else(|| std::env::var(key).ok())
                    .map(|value| (key, value))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let metadata = serde_json::json!({
            "schema_version": 1,
            "started_unix_seconds": started_unix_seconds,
            "sample_interval_seconds": SAMPLE_INTERVAL.as_secs_f64(),
            "logical_cpus": system.cpus().len(),
            "available_parallelism": thread::available_parallelism()?.get(),
            "total_memory_bytes": system.total_memory(),
            "total_swap_bytes": system.total_swap(),
            "os": System::long_os_version(),
            "kernel": System::kernel_version(),
            "cpu_brand": system.cpus().first().map(|cpu| cpu.brand()),
            "profile": profile,
            "command": command.commands.iter().map(|(program, args)| {
                std::iter::once(program).chain(args).map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
            }).collect::<Vec<_>>(),
            "scheduler_env": scheduler_env,
        });
        fs_err::write(
            dir.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        let output = fs_err::File::create(dir.join("samples.jsonl"))?;
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("nextest-resources".into())
            .spawn(move || sample(system, disks, output, started, receiver))?;

        Ok(Self {
            dir: tempdir.keep(),
            stop,
            worker: Some(worker),
        })
    }

    pub(super) fn finish(mut self, exit_code: Option<i32>) -> PathBuf {
        let result = self.stop_worker();
        if let Err(err) = &result {
            log::warn!("nextest resource collection incomplete: {err:#}");
        }
        let outcome = serde_json::json!({
            "collection_complete": result.is_ok(),
            "collection_error": result.err().map(|err| format!("{err:#}")),
            "nextest_exit_code": exit_code,
        });
        if let Err(err) = fs_err::write(
            self.dir.join("outcome.json"),
            serde_json::to_vec_pretty(&outcome).expect("serializable outcome"),
        ) {
            log::warn!("failed to write nextest resource collection outcome: {err:#}");
        }
        log::info!("nextest resource measurements: {}", self.dir.display());
        self.dir.clone()
    }

    fn stop_worker(&mut self) -> anyhow::Result<()> {
        if let Some(worker) = self.worker.take() {
            // A disconnected receiver means the worker already returned an error.
            let _ = self.stop.send(());
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("resource sampler panicked"))??;
        }
        Ok(())
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        if let Err(err) = self.stop_worker() {
            log::warn!("failed to stop nextest resource sampler: {err:#}");
        }
    }
}

fn sample(
    mut system: System,
    mut disks: Disks,
    output: fs_err::File,
    started: Instant,
    stop: mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let mut output = std::io::BufWriter::new(output);
    let mut last_cpu_refresh = started;
    let mut initial = true;
    loop {
        let stopping = if initial {
            false
        } else {
            match stop.recv_timeout(SAMPLE_INTERVAL) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
                Err(mpsc::RecvTimeoutError::Timeout) => false,
            }
        };
        let now = Instant::now();
        let cpu_percent = if !initial
            && now.duration_since(last_cpu_refresh) >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
        {
            system.refresh_cpu_usage();
            last_cpu_refresh = now;
            Some(system.global_cpu_usage())
        } else {
            None
        };
        system.refresh_memory();
        if !initial {
            disks.refresh_specifics(false, DiskRefreshKind::nothing().with_io_usage());
        }
        let disk_usage = disks
            .iter()
            .map(|disk| {
                let usage = disk.usage();
                serde_json::json!({
                    "name": disk.name().to_string_lossy(),
                    "mount_point": disk.mount_point().to_string_lossy(),
                    "read_bytes": usage.total_read_bytes,
                    "written_bytes": usage.total_written_bytes,
                })
            })
            .collect::<Vec<_>>();
        let row = serde_json::json!({
            "elapsed_seconds": started.elapsed().as_secs_f64(),
            "cpu_percent": cpu_percent,
            "used_memory_bytes": system.used_memory(),
            "available_memory_bytes": system.available_memory(),
            "used_swap_bytes": system.used_swap(),
            "disks": disk_usage,
        });
        serde_json::to_writer(&mut output, &row)?;
        writeln!(output)?;
        // Keep completed samples usable if a job is killed before nextest exits.
        output.flush()?;
        if stopping {
            return Ok(());
        }
        initial = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen_cargo_nextest_run_cmd::CommandShell;
    use test_with_tracing::test;

    #[test]
    fn captures_short_failed_runs_and_joins_worker() -> anyhow::Result<()> {
        let config = tempfile::NamedTempFile::new()?;
        fs_err::write(config.path(), "[profile.ci]\n")?;
        let command = Script {
            env: [("NEXTEST_TEST_THREADS".into(), "7".into())].into(),
            commands: vec![("nextest".into(), vec!["run".into()])],
            shell: CommandShell::Bash,
        };
        let monitor = ResourceMonitor::start(config.path(), "ci", &command)?;
        thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL + Duration::from_millis(50));
        let dir = monitor.finish(Some(100));
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs_err::read(dir.join("metadata.json"))?)?;
        assert_eq!(metadata["scheduler_env"]["NEXTEST_TEST_THREADS"], "7");
        assert_eq!(
            fs_err::read_to_string(dir.join("nextest.toml"))?,
            "[profile.ci]\n"
        );
        let rows = fs_err::read_to_string(dir.join("samples.jsonl"))?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert!(rows.len() >= 2);
        assert!(rows[0]["cpu_percent"].is_null());
        assert!(rows[1]["elapsed_seconds"].as_f64() > rows[0]["elapsed_seconds"].as_f64());
        assert!(rows[1]["cpu_percent"].as_f64().is_some());
        let outcome: serde_json::Value =
            serde_json::from_slice(&fs_err::read(dir.join("outcome.json"))?)?;
        assert_eq!(outcome["collection_complete"], true);
        assert_eq!(outcome["nextest_exit_code"], 100);
        fs_err::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn drop_stops_sampler_on_early_return() -> anyhow::Result<()> {
        let config = tempfile::NamedTempFile::new()?;
        let command = Script {
            env: Default::default(),
            commands: vec![],
            shell: CommandShell::Bash,
        };
        let monitor = ResourceMonitor::start(config.path(), "ci", &command)?;
        let dir = monitor.dir.clone();
        drop(monitor);
        let size = fs_err::metadata(dir.join("samples.jsonl"))?.len();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(size, fs_err::metadata(dir.join("samples.jsonl"))?.len());
        assert!(!dir.join("outcome.json").exists());
        fs_err::remove_dir_all(dir)?;
        Ok(())
    }
}
