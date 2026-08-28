// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::command::VirtualClientCommandBuilder;
use super::command::run_command;
use super::config::VmmPerfConfig;
use super::config::VmmPerfProfile;
use super::diagnostics::RunDiagnostics;
use super::host::HostEnvironment;
use super::runtime::VmmPerfRuntime;
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const ITERATIONS: u32 = 1;

pub(crate) struct VirtualClientRunRequest<'a> {
    pub(crate) profile: VmmPerfProfile,
    pub(crate) config: VmmPerfConfig,
    pub(crate) runtime: &'a VmmPerfRuntime,
    pub(crate) openvmm: &'a Path,
    pub(crate) firmware: &'a Path,
    pub(crate) output_dir: &'a Path,
    pub(crate) temp_dir: &'a Path,
    pub(crate) host: &'a HostEnvironment,
    pub(crate) metadata: Option<&'a BTreeMap<String, String>>,
}

pub(crate) struct VirtualClientRun<'a> {
    name: String,
    profile: VmmPerfProfile,
    runtime: &'a VmmPerfRuntime,
    directories: RunDirectories,
    host: &'a HostEnvironment,
    command: Command,
}

impl<'a> VirtualClientRun<'a> {
    pub(crate) fn run(request: VirtualClientRunRequest<'a>) -> ConfigOutcome {
        let name = request.config.name.clone();
        match Self::prepare(request) {
            Ok(mut run) => {
                let (execution, duration_ms) = run.execute();
                run.collect(execution, duration_ms)
            }
            Err(err) => ConfigOutcome::failed(name, err),
        }
    }

    fn prepare(request: VirtualClientRunRequest<'a>) -> anyhow::Result<Self> {
        let mut custom_parameters = request.config.parameters;
        let work_dir_base = custom_parameters.remove("WorkDir");
        let directories = RunDirectories::prepare(
            request.profile,
            &request.config.name,
            request.output_dir,
            request.temp_dir,
            request.runtime.root(),
            work_dir_base.as_deref(),
        )?;
        request.runtime.reset_logs()?;

        let default_backend = if custom_parameters.contains_key("HypervisorBackend") {
            None
        } else {
            Some(request.host.default_hypervisor_backend()?)
        };
        let parameters = resolve_parameters(
            custom_parameters,
            &directories.profile_work_dir,
            request.openvmm,
            request.firmware,
            default_backend,
        );
        request.host.validate_parameters(&parameters)?;
        request
            .host
            .validate_work_dir(&directories.profile_work_dir)?;
        tracing::info!(
            work_dir = %directories.profile_work_dir.display(),
            explicit_base = work_dir_base.is_some(),
            "selected VMM.Perf work directory"
        );

        let experiment_id = experiment_id(request.profile, &request.config.name)?;
        let mut command_builder = VirtualClientCommandBuilder::new(
            request.runtime.root(),
            request.runtime.virtual_client(),
        )
        .profile(request.profile)
        .iterations(ITERATIONS)
        .package_dir(&request.runtime.package_dir())
        .log_dir(&directories.virtual_client_logs)
        .experiment_id(&experiment_id)
        .logger("file")
        .logger("csv")
        .logger("summary")
        .log_to_file(true)
        .temp_dir(&directories.temp_dir);
        if let Some(metadata) = request.metadata {
            for (name, value) in metadata {
                command_builder = command_builder.metadata(name, value);
            }
        }
        for (name, value) in parameters {
            command_builder = command_builder.parameter(name, value);
        }
        let command = command_builder.build()?;

        Ok(Self {
            name: request.config.name,
            profile: request.profile,
            runtime: request.runtime,
            directories,
            host: request.host,
            command,
        })
    }

    fn execute(&mut self) -> (anyhow::Result<std::process::ExitStatus>, u64) {
        tracing::info!(
            configuration = self.name,
            profile = self.profile.name(),
            console_log = %self.directories.console_log_path.display(),
            "starting VMM.Perf configuration"
        );
        let started = Instant::now();
        let execution = run_command(
            &mut self.command,
            &self.directories.console_log_path,
            &self.name,
        );
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        (execution, duration_ms)
    }

    fn collect(
        self,
        execution: anyhow::Result<std::process::ExitStatus>,
        duration_ms: u64,
    ) -> ConfigOutcome {
        let mut errors = Vec::new();
        let (exit_code, process_success) = match execution {
            Ok(status) => {
                if !status.success() {
                    errors.push(format!(
                        "VirtualClient exited with code {}",
                        status.code().unwrap_or(-1)
                    ));
                }
                (status.code(), status.success())
            }
            Err(err) => {
                errors.push(format!("{err:#}"));
                (None, false)
            }
        };

        let directories = &self.directories;
        if let Err(err) = self
            .host
            .restore_ownership(&directories.ownership_paths(self.runtime.root()))
        {
            errors.push(format!("failed to restore file ownership: {err:#}"));
        }
        errors.extend(
            RunDiagnostics {
                config_output_dir: &directories.config_output_dir,
                virtual_client_logs: &directories.virtual_client_logs,
                profile_work_dir: &directories.profile_work_dir,
                temp_dir: &directories.temp_dir,
                runtime_logs: &self.runtime.logs_dir(),
            }
            .collect(process_success),
        );

        let success = process_success && errors.is_empty();
        tracing::info!(
            configuration = self.name,
            profile = self.profile.file(),
            exit_code,
            duration_ms,
            success,
            "VMM.Perf configuration completed"
        );
        ConfigOutcome {
            name: self.name,
            success,
            errors,
        }
    }
}

pub(crate) struct ConfigOutcome {
    name: String,
    success: bool,
    errors: Vec<String>,
}

impl ConfigOutcome {
    pub(crate) fn failure_summary(&self) -> Option<String> {
        (!self.success).then(|| {
            format!(
                "{}: {}",
                self.name,
                self.errors
                    .first()
                    .map(String::as_str)
                    .unwrap_or("unknown failure")
            )
        })
    }

    pub(crate) fn failed(name: String, error: anyhow::Error) -> Self {
        Self {
            name,
            success: false,
            errors: vec![format!("{error:#}")],
        }
    }
}

struct RunDirectories {
    config_output_dir: PathBuf,
    virtual_client_logs: PathBuf,
    console_log_path: PathBuf,
    data_dir: PathBuf,
    profile_work_dir: PathBuf,
    temp_dir: PathBuf,
    _temporary_root: tempfile::TempDir,
    _profile_work_root: Option<tempfile::TempDir>,
}

impl RunDirectories {
    fn prepare(
        profile: VmmPerfProfile,
        config_name: &str,
        output_dir: &Path,
        temp_root: &Path,
        runtime_dir: &Path,
        requested_work_dir_base: Option<&str>,
    ) -> anyhow::Result<Self> {
        let config_output_dir = staged_config_output_dir(output_dir, profile, config_name);
        if config_output_dir.exists() {
            fs_err::remove_dir_all(&config_output_dir)?;
        }
        fs_err::create_dir_all(config_output_dir.join("results"))?;
        fs_err::create_dir_all(config_output_dir.join("openvmm-logs"))?;

        fs_err::create_dir_all(temp_root)?;
        let work = tempfile::Builder::new()
            .prefix(temp_dir_prefix(profile))
            .tempdir_in(temp_root)?;
        let data_dir = work.path().join("data");
        let temp_dir = work.path().join("temp");
        let virtual_client_logs = work.path().join("virtual-client");
        let console_log_path = virtual_client_logs.join("console.log");
        fs_err::create_dir_all(&data_dir)?;
        fs_err::create_dir_all(&temp_dir)?;
        fs_err::create_dir_all(&virtual_client_logs)?;

        let (profile_work_dir, profile_work_root) = match requested_work_dir_base {
            Some(base) => {
                let base = resolve_work_dir_base(base, runtime_dir)?;
                let profile_work_root = tempfile::Builder::new()
                    .prefix(&format!("vmm-perf-{}-{config_name}-", profile.name()))
                    .tempdir_in(&base)
                    .with_context(|| {
                        format!(
                            "failed to create a VMM.Perf work directory under {}",
                            base.display()
                        )
                    })?;
                (profile_work_root.path().to_owned(), Some(profile_work_root))
            }
            None => (data_dir.clone(), None),
        };

        Ok(Self {
            config_output_dir,
            virtual_client_logs,
            console_log_path,
            profile_work_dir,
            data_dir,
            temp_dir,
            _temporary_root: work,
            _profile_work_root: profile_work_root,
        })
    }

    fn ownership_paths<'a>(&'a self, runtime_dir: &'a Path) -> Vec<&'a Path> {
        let mut paths = Vec::new();
        for path in [
            runtime_dir,
            self.data_dir.as_path(),
            self.temp_dir.as_path(),
            self.config_output_dir.as_path(),
            self.profile_work_dir.as_path(),
        ] {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }
}

fn temp_dir_prefix(profile: VmmPerfProfile) -> &'static str {
    match profile {
        VmmPerfProfile::Fio => "f-",
        VmmPerfProfile::Iperf3 => "i-",
        VmmPerfProfile::BootTime => "b-",
    }
}

fn staged_config_output_dir(
    output_dir: &Path,
    profile: VmmPerfProfile,
    config_name: &str,
) -> PathBuf {
    output_dir.join(profile.name()).join(config_name)
}

fn resolve_parameters(
    custom: BTreeMap<String, String>,
    profile_work_dir: &Path,
    openvmm: &Path,
    firmware: &Path,
    default_backend: Option<&str>,
) -> BTreeMap<String, String> {
    let mut parameters = BTreeMap::from([
        ("WorkDir".into(), profile_work_dir.display().to_string()),
        ("OpenVmmBinary".into(), openvmm.display().to_string()),
        ("MsvmFirmware".into(), firmware.display().to_string()),
    ]);
    if let Some(default_backend) = default_backend {
        parameters.insert("HypervisorBackend".into(), default_backend.into());
    }
    parameters.extend(custom);
    parameters
}

fn resolve_work_dir_base(base: &str, runtime_dir: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !base.trim().is_empty(),
        "VMM.Perf WorkDir base cannot be empty"
    );
    let base = PathBuf::from(base);
    let base = if base.is_absolute() {
        base
    } else {
        runtime_dir.join(base)
    };
    let metadata = fs_err::metadata(&base)
        .with_context(|| format!("VMM.Perf WorkDir base does not exist: {}", base.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "VMM.Perf WorkDir base is not a directory: {}",
        base.display()
    );
    fs_err::canonicalize(&base)
        .with_context(|| format!("failed to resolve VMM.Perf WorkDir base {}", base.display()))
}

fn experiment_id(profile: VmmPerfProfile, config_name: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{}-{config_name}-{}",
        profile.name(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ))
}

#[cfg(test)]
mod tests {
    use super::resolve_parameters;
    use super::resolve_work_dir_base;
    use super::staged_config_output_dir;
    use crate::config::VmmPerfProfile;
    use crate::test_support;
    use std::collections::BTreeMap;
    use std::path::Path;
    use test_with_tracing::test;

    #[test]
    fn resolves_relative_work_dir_base_against_runtime_dir() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("work-dir-base")?;
        let runtime_dir = scratch.path().join("runtime");
        let work_base = runtime_dir.join("relative-base");

        fs_err::create_dir_all(&work_base)?;

        assert_eq!(
            resolve_work_dir_base("relative-base", &runtime_dir)?,
            fs_err::canonicalize(&work_base)?
        );
        Ok(())
    }

    #[test]
    fn stages_profile_specific_output_paths() {
        assert_eq!(
            staged_config_output_dir(Path::new("out"), VmmPerfProfile::BootTime, "cfg"),
            Path::new("out").join("boot_time").join("cfg")
        );
    }

    #[test]
    fn resolves_parameters_with_default_backend() {
        let parameters = resolve_parameters(
            BTreeMap::from([("CpuCount".into(), "4".into())]),
            Path::new("work"),
            Path::new("openvmm"),
            Path::new("firmware"),
            Some("kvm"),
        );

        assert_eq!(parameters["WorkDir"], "work");
        assert_eq!(parameters["OpenVmmBinary"], "openvmm");
        assert_eq!(parameters["MsvmFirmware"], "firmware");
        assert_eq!(parameters["HypervisorBackend"], "kvm");
        assert_eq!(parameters["CpuCount"], "4");
    }
}
