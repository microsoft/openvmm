// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::config::ConfigSelection;
use crate::config::VmmPerfProfile;
use clap::ArgAction;
use clap::Parser;
use clap::ValueEnum;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "vmm_perf",
    about = "Run VMM.Perf profiles against an OpenVMM build"
)]
pub(crate) struct Cli {
    #[arg(long, value_name = "PATH")]
    pub(crate) openvmm: PathBuf,

    #[arg(long, value_name = "PATH")]
    pub(crate) firmware: PathBuf,

    #[arg(long = "runtime-archive", value_name = "PATH")]
    pub(crate) runtime_archive: PathBuf,

    #[arg(long = "output-dir", value_name = "DIR")]
    pub(crate) output_dir: PathBuf,

    #[arg(long = "temp-dir", value_name = "DIR")]
    pub(crate) temp_dir: Option<PathBuf>,

    #[arg(
        long = "profile",
        value_name = "PROFILE",
        value_enum,
        action = ArgAction::Append
    )]
    profiles: Vec<ProfileArg>,

    #[arg(long = "vm-sizes-json", value_name = "JSON")]
    pub(crate) vm_sizes_json: Option<String>,

    #[arg(long = "parameters-json", value_name = "JSON")]
    pub(crate) parameters_json: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProfileArg {
    #[value(name = "fio")]
    Fio,
    #[value(name = "iperf3")]
    Iperf3,
    #[value(name = "boot-time")]
    BootTime,
}

impl Cli {
    pub(crate) fn selected_profiles(&self) -> Vec<VmmPerfProfile> {
        if self.profiles.is_empty() {
            return VmmPerfProfile::all().to_vec();
        }

        let mut profiles = Vec::new();
        for profile in &self.profiles {
            let profile = match profile {
                ProfileArg::Fio => VmmPerfProfile::Fio,
                ProfileArg::Iperf3 => VmmPerfProfile::Iperf3,
                ProfileArg::BootTime => VmmPerfProfile::BootTime,
            };
            if !profiles.contains(&profile) {
                profiles.push(profile);
            }
        }
        profiles
    }

    pub(crate) fn config_selection(&self) -> ConfigSelection {
        ConfigSelection {
            vm_sizes_json: self.vm_sizes_json.clone(),
            parameters_json: self.parameters_json.clone(),
        }
    }
}

pub(crate) fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vmm_perf=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use crate::config::VmmPerfProfile;
    use clap::Parser as _;
    use test_with_tracing::test;

    #[test]
    fn defaults_to_all_profiles() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from([
            "vmm_perf",
            "--openvmm",
            "openvmm",
            "--firmware",
            "firmware",
            "--runtime-archive",
            "runtime.tar.gz",
            "--output-dir",
            "out",
        ])?;

        assert_eq!(cli.selected_profiles(), VmmPerfProfile::all());
        assert!(cli.temp_dir.is_none());
        assert!(cli.vm_sizes_json.is_none());
        assert!(cli.parameters_json.is_none());
        Ok(())
    }

    #[test]
    fn parses_repeated_profiles_and_json_overrides() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from([
            "vmm_perf",
            "--openvmm",
            "openvmm",
            "--firmware",
            "firmware",
            "--runtime-archive",
            "runtime.tar.gz",
            "--output-dir",
            "out",
            "--profile",
            "fio",
            "--profile",
            "boot-time",
            "--profile",
            "fio",
            "--vm-sizes-json",
            "[]",
            "--parameters-json",
            "{}",
        ])?;

        assert_eq!(
            cli.selected_profiles(),
            vec![VmmPerfProfile::Fio, VmmPerfProfile::BootTime]
        );
        assert_eq!(cli.vm_sizes_json.as_deref(), Some("[]"));
        assert_eq!(cli.parameters_json.as_deref(), Some("{}"));
        Ok(())
    }

    #[test]
    fn rejects_legacy_boot_time_profile_spelling() {
        let error = Cli::try_parse_from([
            "vmm_perf",
            "--openvmm",
            "openvmm",
            "--firmware",
            "firmware",
            "--runtime-archive",
            "runtime.tar.gz",
            "--output-dir",
            "out",
            "--profile",
            "boot_time",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("boot-time"));
    }
}
