// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::config::VmmPerfProfile;
use super::host::platform_command;
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) struct VirtualClientCommandBuilder<'a> {
    runtime_dir: &'a Path,
    virtual_client: &'a Path,
    profile: Option<VmmPerfProfile>,
    iterations: Option<u32>,
    parameters: BTreeMap<String, String>,
    package_dir: Option<PathBuf>,
    log_dir: Option<&'a Path>,
    experiment_id: Option<String>,
    loggers: Vec<&'a str>,
    log_to_file: bool,
    temp_dir: Option<&'a Path>,
}

impl<'a> VirtualClientCommandBuilder<'a> {
    pub(crate) fn new(runtime_dir: &'a Path, virtual_client: &'a Path) -> Self {
        Self {
            runtime_dir,
            virtual_client,
            profile: None,
            iterations: None,
            parameters: BTreeMap::new(),
            package_dir: None,
            log_dir: None,
            experiment_id: None,
            loggers: Vec::new(),
            log_to_file: false,
            temp_dir: None,
        }
    }

    pub(crate) fn profile(mut self, profile: VmmPerfProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    pub(crate) fn iterations(mut self, iterations: u32) -> Self {
        self.iterations = Some(iterations);
        self
    }

    pub(crate) fn parameter(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parameters.insert(name.into(), value.into());
        self
    }

    pub(crate) fn package_dir(mut self, package_dir: &Path) -> Self {
        self.package_dir = Some(package_dir.to_owned());
        self
    }

    pub(crate) fn log_dir(mut self, log_dir: &'a Path) -> Self {
        self.log_dir = Some(log_dir);
        self
    }

    pub(crate) fn experiment_id(mut self, experiment_id: &str) -> Self {
        self.experiment_id = Some(experiment_id.to_owned());
        self
    }

    pub(crate) fn logger(mut self, logger: &'a str) -> Self {
        self.loggers.push(logger);
        self
    }

    pub(crate) fn log_to_file(mut self, log_to_file: bool) -> Self {
        self.log_to_file = log_to_file;
        self
    }

    pub(crate) fn temp_dir(mut self, temp_dir: &'a Path) -> Self {
        self.temp_dir = Some(temp_dir);
        self
    }

    pub(crate) fn build(self) -> anyhow::Result<Command> {
        let profile = self.profile.context("VirtualClient profile was not set")?;
        let iterations = self
            .iterations
            .context("VirtualClient iterations were not set")?;
        let package_dir = self
            .package_dir
            .context("VirtualClient package directory was not set")?;
        let log_dir = self
            .log_dir
            .context("VirtualClient log directory was not set")?;
        let experiment_id = self
            .experiment_id
            .context("VirtualClient experiment ID was not set")?;
        let temp_dir = self
            .temp_dir
            .context("VirtualClient temp directory was not set")?;
        anyhow::ensure!(
            !self.loggers.is_empty(),
            "VirtualClient requires at least one logger"
        );
        let env = [("TEMP", temp_dir), ("TMP", temp_dir), ("TMPDIR", temp_dir)];
        let mut command = platform_command(self.virtual_client, &env)?;

        command
            .current_dir(self.runtime_dir)
            .arg(format!(
                "--profile={}",
                self.runtime_dir
                    .join("profiles")
                    .join(profile.file())
                    .display()
            ))
            .arg(format!("--iterations={iterations}"))
            .arg(format!("--package-dir={}", package_dir.display()))
            .arg(format!("--log-dir={}", log_dir.display()))
            .arg(format!("--experiment-id={experiment_id}"));
        for (name, value) in self.parameters {
            command.arg(format!("--parameters={name}={value}"));
        }
        for logger in self.loggers {
            command.arg(format!("--logger={logger}"));
        }
        if self.log_to_file {
            command.arg("--log-to-file");
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command)
    }
}

pub(crate) fn run_command(
    command: &mut Command,
    console_log: petri::PetriLogFile,
) -> anyhow::Result<std::process::ExitStatus> {
    let mut child = command
        .spawn()
        .context("failed to launch VMM.Perf VirtualClient")?;
    let stdout = child
        .stdout
        .take()
        .context("VMM.Perf VirtualClient stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("VMM.Perf VirtualClient stderr was not piped")?;
    let console_log = Arc::new(Mutex::new(console_log));
    let stderr_log = Arc::clone(&console_log);

    std::thread::scope(|scope| {
        let stdout_task = scope.spawn(move || log_process_output("stdout", stdout, console_log));
        let stderr_task = scope.spawn(move || log_process_output("stderr", stderr, stderr_log));
        let status = child
            .wait()
            .context("failed to wait for VMM.Perf VirtualClient");
        let stdout_result = stdout_task.join().unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "VMM.Perf VirtualClient stdout logging thread panicked"
            ))
        });
        let stderr_result = stderr_task.join().unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "VMM.Perf VirtualClient stderr logging thread panicked"
            ))
        });
        let status = status?;
        stdout_result?;
        stderr_result?;
        Ok(status)
    })
}

fn log_process_output(
    stream_name: &str,
    stream: impl Read,
    log_file: Arc<Mutex<petri::PetriLogFile>>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("failed to read VMM.Perf VirtualClient {stream_name}"))?;
        if bytes_read == 0 {
            return Ok(());
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let log_file = log_file
            .lock()
            .map_err(|_| anyhow::anyhow!("VMM.Perf console log mutex was poisoned"))?;
        log_file.write_entry(String::from_utf8_lossy(&line));
    }
}
