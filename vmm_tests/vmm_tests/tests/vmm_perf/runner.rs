// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::config::VmmPerfProfile;
use super::config::selected_configs;
use super::host::HostEnvironment;
use super::runtime::VmmPerfRuntime;
use super::virtual_client::VirtualClientRun;
use super::virtual_client::VirtualClientRunRequest;
use std::path::PathBuf;

pub(crate) struct VmmPerfArtifacts {
    pub(crate) openvmm: petri::ResolvedArtifact,
    pub(crate) firmware: petri::ResolvedArtifact,
    pub(crate) runtime_archive: petri::ResolvedArtifact,
    pub(crate) log_dir: petri::ResolvedArtifact,
}

pub(crate) struct VmmPerfRunner<'a> {
    logger: &'a petri::PetriLogSource,
    runtime: VmmPerfRuntime,
    host: HostEnvironment,
    openvmm: PathBuf,
    firmware: PathBuf,
    output_dir: PathBuf,
}

impl<'a> VmmPerfRunner<'a> {
    pub(crate) fn new(
        params: petri::PetriTestParams<'a>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<Self> {
        let openvmm = artifacts.openvmm.get().to_owned();
        let firmware = artifacts.firmware.get().to_owned();
        let output_dir = artifacts.log_dir.get().to_owned();
        ensure_file(&openvmm, "OpenVMM executable")?;
        ensure_file(&firmware, "MSVM firmware")?;
        fs_err::create_dir_all(&output_dir)?;
        let host = HostEnvironment::detect()?;

        Ok(Self {
            logger: params.logger,
            runtime: VmmPerfRuntime::prepare(artifacts.runtime_archive.get())?,
            host,
            openvmm,
            firmware,
            output_dir,
        })
    }

    pub(crate) fn run(&self, profile: VmmPerfProfile) -> anyhow::Result<()> {
        self.runtime.validate_profile(profile)?;
        let outcomes = selected_configs(self.host.capacity()?)?
            .into_iter()
            .map(|config| {
                VirtualClientRun::run(VirtualClientRunRequest {
                    profile,
                    config,
                    runtime: &self.runtime,
                    openvmm: &self.openvmm,
                    firmware: &self.firmware,
                    output_dir: &self.output_dir,
                    logger: self.logger,
                    host: &self.host,
                })
            })
            .collect::<Vec<_>>();

        let failures = outcomes
            .iter()
            .filter_map(|outcome| outcome.failure_summary())
            .collect::<Vec<_>>();
        anyhow::ensure!(
            failures.is_empty(),
            "VMM.Perf profile {} failed for {} configuration(s): {}",
            profile.file(),
            failures.len(),
            failures.join("; ")
        );
        Ok(())
    }
}

fn ensure_file(path: &std::path::Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "{description} does not exist or is not a file: {}",
        path.display()
    );
    Ok(())
}
