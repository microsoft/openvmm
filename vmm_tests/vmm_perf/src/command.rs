// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::config::VmmPerfProfile;
use super::host::platform_command;
use anyhow::Context as _;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;

pub(crate) struct VirtualClientCommandBuilder<'a> {
    runtime_dir: &'a Path,
    virtual_client: &'a Path,
    profile: Option<VmmPerfProfile>,
    iterations: Option<u32>,
    metadata: BTreeMap<String, String>,
    parameters: BTreeMap<String, String>,
    package_dir: Option<PathBuf>,
    log_dir: Option<&'a Path>,
    experiment_id: Option<String>,
    loggers: Vec<&'a str>,
    log_to_file: bool,
    temp_dir: Option<&'a Path>,
}

#[derive(Debug, Eq, PartialEq)]
struct VirtualClientInvocation {
    program: PathBuf,
    current_dir: PathBuf,
    args: Vec<String>,
    env: Vec<(String, PathBuf)>,
}

impl<'a> VirtualClientCommandBuilder<'a> {
    pub(crate) fn new(runtime_dir: &'a Path, virtual_client: &'a Path) -> Self {
        Self {
            runtime_dir,
            virtual_client,
            profile: None,
            iterations: None,
            metadata: BTreeMap::new(),
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

    pub(crate) fn metadata(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(name.into(), value.into());
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
        self.build_invocation()?.into_command()
    }

    fn build_invocation(self) -> anyhow::Result<VirtualClientInvocation> {
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

        let mut args = vec![
            format!(
                "--profile={}",
                self.runtime_dir
                    .join("profiles")
                    .join(profile.file())
                    .display()
            ),
            format!("--iterations={iterations}"),
            format!("--package-dir={}", package_dir.display()),
            format!("--log-dir={}", log_dir.display()),
            format!("--experiment-id={experiment_id}"),
        ];
        if !self.metadata.is_empty() {
            let metadata = self
                .metadata
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(",,,");
            args.push(format!("--metadata={metadata}"));
        }
        for (name, value) in self.parameters {
            args.push(format!("--parameters={name}={value}"));
        }
        for logger in self.loggers {
            args.push(format!("--logger={logger}"));
        }
        if self.log_to_file {
            args.push("--log-to-file".into());
        }

        Ok(VirtualClientInvocation {
            program: self.virtual_client.to_owned(),
            current_dir: self.runtime_dir.to_owned(),
            args,
            env: vec![
                ("TEMP".into(), temp_dir.to_owned()),
                ("TMP".into(), temp_dir.to_owned()),
                ("TMPDIR".into(), temp_dir.to_owned()),
            ],
        })
    }
}

impl VirtualClientInvocation {
    fn into_command(self) -> anyhow::Result<Command> {
        let env = self
            .env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_path()))
            .collect::<Vec<_>>();
        let mut command = platform_command(&self.program, &env)?;
        command.current_dir(&self.current_dir);
        command.args(&self.args);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command)
    }
}

pub(crate) fn run_command(
    command: &mut Command,
    console_log_path: &Path,
    config_name: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let log_file = fs_err::File::create(console_log_path).with_context(|| {
        format!(
            "failed to create VMM.Perf console log {}",
            console_log_path.display()
        )
    })?;
    let log_file = Arc::new(Mutex::new(BufWriter::new(log_file)));
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
    let stdout_log = Arc::clone(&log_file);
    let stderr_log = Arc::clone(&log_file);

    std::thread::scope(|scope| {
        let stdout_task =
            scope.spawn(move || log_process_output("stdout", stdout, config_name, stdout_log));
        let stderr_task =
            scope.spawn(move || log_process_output("stderr", stderr, config_name, stderr_log));
        let status = child
            .wait()
            .context("failed to wait for VMM.Perf VirtualClient");
        let stdout_result = match stdout_task.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "VMM.Perf VirtualClient stdout logging thread panicked"
            )),
        };
        let stderr_result = match stderr_task.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "VMM.Perf VirtualClient stderr logging thread panicked"
            )),
        };
        let flush_result = log_file.lock().flush().with_context(|| {
            format!(
                "failed to flush VMM.Perf console log {}",
                console_log_path.display()
            )
        });
        let status = status?;
        stdout_result?;
        stderr_result?;
        flush_result?;
        Ok(status)
    })
}

fn log_process_output(
    stream_name: &str,
    stream: impl Read,
    config_name: &str,
    log_file: Arc<Mutex<BufWriter<fs_err::File>>>,
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
        let line = String::from_utf8_lossy(&line).into_owned();
        tracing::debug!(
            configuration = config_name,
            stream = stream_name,
            line = %line,
            "VirtualClient output"
        );
        let mut log_file = log_file.lock();
        writeln!(log_file, "[{stream_name}] {line}")
            .with_context(|| format!("failed to write VMM.Perf {stream_name} console output"))?;
        log_file
            .flush()
            .with_context(|| format!("failed to flush VMM.Perf {stream_name} console output"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::VirtualClientCommandBuilder;
    use crate::config::VmmPerfProfile;
    use std::path::Path;
    use test_with_tracing::test;

    #[test]
    fn builds_virtual_client_arguments() -> anyhow::Result<()> {
        let runtime_dir = Path::new("runtime");
        let log_dir = Path::new("out");
        let invocation = VirtualClientCommandBuilder::new(
            runtime_dir,
            &runtime_dir.join(if cfg!(target_os = "windows") {
                "VirtualClient.exe"
            } else {
                "VirtualClient"
            }),
        )
        .profile(VmmPerfProfile::Fio)
        .iterations(1)
        .parameter("CpuCount", "4")
        .parameter("MemoryMB", "4096")
        .package_dir(&runtime_dir.join("packages"))
        .log_dir(log_dir)
        .experiment_id("exp-1")
        .logger("file")
        .logger("summary")
        .log_to_file(true)
        .temp_dir(Path::new("temp"))
        .build_invocation()?;

        assert!(invocation.args.contains(&format!(
            "--profile={}",
            runtime_dir
                .join("profiles")
                .join(VmmPerfProfile::Fio.file())
                .display()
        )));
        assert!(invocation.args.contains(&"--iterations=1".to_string()));
        assert!(invocation.args.contains(&format!(
            "--package-dir={}",
            runtime_dir.join("packages").display()
        )));
        assert!(
            invocation
                .args
                .contains(&format!("--log-dir={}", log_dir.display()))
        );
        assert!(
            invocation
                .args
                .contains(&"--experiment-id=exp-1".to_string())
        );
        assert!(
            invocation
                .args
                .contains(&"--parameters=CpuCount=4".to_string())
        );
        assert!(
            invocation
                .args
                .contains(&"--parameters=MemoryMB=4096".to_string())
        );
        assert!(invocation.args.contains(&"--logger=file".to_string()));
        assert!(invocation.args.contains(&"--logger=summary".to_string()));
        assert!(invocation.args.contains(&"--log-to-file".to_string()));
        assert_eq!(
            invocation.env,
            vec![
                ("TEMP".into(), Path::new("temp").to_path_buf()),
                ("TMP".into(), Path::new("temp").to_path_buf()),
                ("TMPDIR".into(), Path::new("temp").to_path_buf()),
            ]
        );
        Ok(())
    }
}
