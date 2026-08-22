// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::cli::Cli;
use super::config::VmmPerfConfig;
use super::config::VmmPerfProfile;
use super::config::selected_configs;
use super::host::HostEnvironment;
use super::runtime::VmmPerfRuntime;
use super::virtual_client::VirtualClientRun;
use super::virtual_client::VirtualClientRunRequest;
use std::path::Path;
use std::path::PathBuf;

pub(crate) struct VmmPerfRunner {
    runtime: VmmPerfRuntime,
    host: HostEnvironment,
    openvmm: PathBuf,
    firmware: PathBuf,
    output_dir: PathBuf,
    temp_dir: PathBuf,
    profiles: Vec<VmmPerfProfile>,
    configs: Vec<VmmPerfConfig>,
}

impl VmmPerfRunner {
    pub(crate) fn new(cli: Cli) -> anyhow::Result<Self> {
        ensure_file(&cli.openvmm, "OpenVMM executable")?;
        ensure_file(&cli.firmware, "MSVM firmware")?;
        fs_err::create_dir_all(&cli.output_dir)?;
        let host = HostEnvironment::detect()?;
        let profiles = cli.selected_profiles();
        let configs = selected_configs(host.capacity()?, &cli.config_selection())?;
        let temp_dir = cli.temp_dir.unwrap_or_else(std::env::temp_dir);
        fs_err::create_dir_all(&temp_dir)?;

        tracing::info!(
            output_dir = %cli.output_dir.display(),
            profile_count = profiles.len(),
            configuration_count = configs.len(),
            "prepared VMM.Perf run"
        );

        Ok(Self {
            runtime: VmmPerfRuntime::prepare(&cli.runtime_archive)?,
            host,
            openvmm: cli.openvmm,
            firmware: cli.firmware,
            output_dir: cli.output_dir,
            temp_dir,
            profiles,
            configs,
        })
    }

    pub(crate) fn run(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for profile in &self.profiles {
            tracing::info!(profile = profile.name(), "running VMM.Perf profile");
            failures.extend(self.run_profile(*profile));
        }

        anyhow::ensure!(
            failures.is_empty(),
            "VMM.Perf failed for {} profile/configuration(s): {}",
            failures.len(),
            failures.join("; ")
        );
        Ok(())
    }

    fn run_profile(&self, profile: VmmPerfProfile) -> Vec<String> {
        if let Err(err) = self.runtime.validate_profile(profile) {
            return vec![format!("{}: {err:#}", profile.name())];
        }

        self.configs
            .iter()
            .cloned()
            .filter_map(|config| {
                VirtualClientRun::run(VirtualClientRunRequest {
                    profile,
                    config,
                    runtime: &self.runtime,
                    openvmm: &self.openvmm,
                    firmware: &self.firmware,
                    output_dir: &self.output_dir,
                    temp_dir: &self.temp_dir,
                    host: &self.host,
                })
                .failure_summary()
                .map(|summary| format!("{} / {summary}", profile.name()))
            })
            .collect()
    }
}

fn ensure_file(path: &Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "{description} does not exist or is not a file: {}",
        path.display()
    );
    Ok(())
}
