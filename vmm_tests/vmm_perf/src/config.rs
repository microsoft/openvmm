// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::host::HostCapacity;
use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const MIB: u64 = 1024 * 1024;
const MIB_PER_GIB: u64 = 1024;
const DEFAULT_CAPACITY_PERCENT: u64 = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmmPerfProfile {
    Fio,
    Iperf3,
    BootTime,
}

impl VmmPerfProfile {
    const ALL: [Self; 3] = [Self::Fio, Self::Iperf3, Self::BootTime];

    pub(crate) fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fio => "fio",
            Self::Iperf3 => "iperf3",
            Self::BootTime => "boot_time",
        }
    }

    pub(crate) fn file(self) -> String {
        self.file_for(std::env::consts::ARCH, std::env::consts::OS)
    }

    fn file_for(self, architecture: &str, operating_system: &str) -> String {
        let architecture = match architecture {
            "x86_64" => "X64",
            "aarch64" => "ARM64",
            _ => unreachable!("unsupported VMM.Perf host architecture"),
        };
        let platform = match operating_system {
            "linux" => "LINUX",
            "windows" => "WIN",
            _ => unreachable!("unsupported VMM.Perf host operating system"),
        };
        let boot_mode = "UEFI";
        let profile = match self {
            Self::Fio => "FIO",
            Self::Iperf3 => "IPERF3",
            Self::BootTime => "BOOTTIME",
        };
        format!("PERF-OPENVMM-{architecture}-{platform}-{boot_mode}-{profile}.json")
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigSelection {
    pub(crate) vm_sizes_json: Option<String>,
    pub(crate) parameters_json: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct VmmPerfConfig {
    pub(crate) name: String,
    pub(crate) parameters: BTreeMap<String, String>,
    name_is_explicit: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    name: Option<String>,
    parameters: BTreeMap<String, serde_json::Value>,
}

pub(crate) fn selected_configs(
    capacity: HostCapacity,
    selection: &ConfigSelection,
) -> anyhow::Result<Vec<VmmPerfConfig>> {
    let raw_configs =
        parse_json::<Vec<RawConfig>>(selection.vm_sizes_json.as_deref(), "--vm-sizes-json")?
            .unwrap_or_default();
    let mut configs = if raw_configs.is_empty() {
        default_configs(capacity)?
    } else {
        configs_from_raw(raw_configs)?
    };

    if let Some(parameters) = parse_json::<BTreeMap<String, serde_json::Value>>(
        selection.parameters_json.as_deref(),
        "--parameters-json",
    )? {
        apply_parameters(
            &mut configs,
            stringify_parameters(parameters, "--parameters-json")?,
        )?;
    }
    Ok(configs)
}

fn configs_from_raw(raw: Vec<RawConfig>) -> anyhow::Result<Vec<VmmPerfConfig>> {
    let mut names = BTreeSet::new();
    raw.into_iter()
        .enumerate()
        .map(|(source_index, config)| {
            let parameters = stringify_parameters(
                config.parameters,
                &format!("VMM.Perf configuration {}", source_index + 1),
            )?;

            let name_is_explicit = config.name.is_some();
            let name = match config.name.as_deref() {
                Some(name) => validate_name(name)?,
                None => generated_config_name(&parameters, source_index),
            };
            anyhow::ensure!(
                names.insert(name.clone()),
                "duplicate VMM.Perf configuration name {name:?}"
            );

            Ok(VmmPerfConfig {
                name,
                parameters,
                name_is_explicit,
            })
        })
        .collect()
}

fn apply_parameters(
    configs: &mut [VmmPerfConfig],
    parameters: BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (index, config) in configs.iter_mut().enumerate() {
        config.parameters.extend(parameters.clone());
        if !config.name_is_explicit {
            config.name = generated_config_name(&config.parameters, index);
        }
    }

    let mut names = BTreeSet::new();
    for config in configs {
        anyhow::ensure!(
            names.insert(config.name.clone()),
            "duplicate VMM.Perf configuration name {:?}",
            config.name
        );
    }
    Ok(())
}

fn parse_json<T: DeserializeOwned>(json: Option<&str>, source: &str) -> anyhow::Result<Option<T>> {
    let Some(json) = json else {
        return Ok(None);
    };
    serde_json::from_str(json)
        .with_context(|| format!("failed to parse {source}"))
        .map(Some)
}

fn stringify_parameters(
    parameters: BTreeMap<String, serde_json::Value>,
    context: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    parameters
        .into_iter()
        .map(|(name, value)| {
            let name = name.trim();
            anyhow::ensure!(
                !name.is_empty(),
                "{context} contains an empty parameter name"
            );
            Ok((name.to_owned(), scalar_to_string(name, &value)?))
        })
        .collect()
}

fn default_configs(capacity: HostCapacity) -> anyhow::Result<Vec<VmmPerfConfig>> {
    let cpu_count = capacity
        .logical_processors
        .saturating_mul(DEFAULT_CAPACITY_PERCENT as usize)
        / 100;
    let cpu_count = u64::try_from(cpu_count.max(1)).context("host processor count is too large")?;
    let memory_mb = capacity
        .available_memory_bytes
        .saturating_mul(DEFAULT_CAPACITY_PERCENT)
        / 100
        / MIB
        / MIB_PER_GIB
        * MIB_PER_GIB;
    let memory_mb = memory_mb.max(MIB_PER_GIB);

    Ok(vec![VmmPerfConfig {
        name: shape_name(cpu_count, memory_mb),
        parameters: BTreeMap::from([
            ("CpuCount".into(), cpu_count.to_string()),
            ("MemoryMB".into(), memory_mb.to_string()),
        ]),
        name_is_explicit: false,
    }])
}

fn generated_config_name(parameters: &BTreeMap<String, String>, index: usize) -> String {
    let cpu_count = parameters
        .get("CpuCount")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let memory_mb = parameters
        .get("MemoryMB")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);

    match (cpu_count, memory_mb) {
        (Some(cpu_count), Some(memory_mb)) => shape_name(cpu_count, memory_mb),
        _ => format!("config-{}", index + 1),
    }
}

fn shape_name(cpu_count: u64, memory_mb: u64) -> String {
    format!("cpu-{cpu_count}-memory-{memory_mb}mb")
}

fn validate_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    anyhow::ensure!(
        !name.is_empty(),
        "VMM.Perf configuration name cannot be empty"
    );
    anyhow::ensure!(
        !matches!(name, "." | ".."),
        "VMM.Perf configuration name cannot be '.' or '..'"
    );
    anyhow::ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "VMM.Perf configuration name {name:?} may contain only ASCII letters, numbers, '-', '_', or '.'"
    );
    Ok(name.to_owned())
}

fn scalar_to_string(name: &str, value: &serde_json::Value) -> anyhow::Result<String> {
    match value {
        serde_json::Value::String(value) => Ok(value.clone()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        _ => anyhow::bail!("VMM.Perf parameter {name:?} must be a string, number, or boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigSelection;
    use super::VmmPerfProfile;
    use super::selected_configs;
    use super::validate_name;
    use crate::host::HostCapacity;
    use std::collections::BTreeMap;
    use test_with_tracing::test;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn default_configs_use_eighty_percent_capacity() -> anyhow::Result<()> {
        let configs = selected_configs(
            HostCapacity {
                logical_processors: 10,
                available_memory_bytes: 10 * GIB,
            },
            &ConfigSelection::default(),
        )?;

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "cpu-8-memory-8192mb");
        assert_eq!(
            configs[0].parameters,
            BTreeMap::from([
                ("CpuCount".into(), "8".into()),
                ("MemoryMB".into(), "8192".into()),
            ])
        );
        Ok(())
    }

    #[test]
    fn vm_sizes_and_parameters_json_preserve_override_behavior() -> anyhow::Result<()> {
        let configs = selected_configs(
            HostCapacity {
                logical_processors: 16,
                available_memory_bytes: 32 * GIB,
            },
            &ConfigSelection {
                vm_sizes_json: Some(
                    r#"[{"parameters":{"CpuCount":4,"MemoryMB":4096}},{"name":"custom","parameters":{"CpuCount":2,"MemoryMB":2048}}]"#
                        .into(),
                ),
                parameters_json: Some(r#"{"CpuCount":6,"HypervisorBackend":"kvm"}"#.into()),
            },
        )?;

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "cpu-6-memory-4096mb");
        assert_eq!(configs[0].parameters["CpuCount"], "6");
        assert_eq!(configs[0].parameters["HypervisorBackend"], "kvm");
        assert_eq!(configs[1].name, "custom");
        assert_eq!(configs[1].parameters["CpuCount"], "6");
        Ok(())
    }

    #[test]
    fn rejects_non_scalar_parameter_values() {
        let error = selected_configs(
            HostCapacity {
                logical_processors: 16,
                available_memory_bytes: 32 * GIB,
            },
            &ConfigSelection {
                vm_sizes_json: Some(r#"[{"parameters":{"CpuCount":[4]}}]"#.into()),
                parameters_json: None,
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("must be a string, number, or boolean"));
    }

    #[test]
    fn trims_parameter_names() -> anyhow::Result<()> {
        let configs = selected_configs(
            HostCapacity {
                logical_processors: 16,
                available_memory_bytes: 32 * GIB,
            },
            &ConfigSelection {
                vm_sizes_json: None,
                parameters_json: Some(r#"{" CpuCount ":6}"#.into()),
            },
        )?;

        assert_eq!(configs[0].parameters["CpuCount"], "6");
        assert!(!configs[0].parameters.contains_key(" CpuCount "));
        Ok(())
    }

    #[test]
    fn rejects_special_path_component_names() -> anyhow::Result<()> {
        for name in [".", "..", " . ", " .. "] {
            assert!(validate_name(name).is_err());
        }
        assert_eq!(validate_name("config..large")?, "config..large");
        Ok(())
    }

    #[test]
    fn builds_profile_file_names_for_supported_platforms() {
        for (profile, suffix) in [
            (VmmPerfProfile::Fio, "FIO"),
            (VmmPerfProfile::Iperf3, "IPERF3"),
            (VmmPerfProfile::BootTime, "BOOTTIME"),
        ] {
            assert_eq!(
                profile.file_for("x86_64", "linux"),
                format!("PERF-OPENVMM-X64-LINUX-UEFI-{suffix}.json")
            );
            assert_eq!(
                profile.file_for("aarch64", "linux"),
                format!("PERF-OPENVMM-ARM64-LINUX-UEFI-{suffix}.json")
            );
            assert_eq!(
                profile.file_for("x86_64", "windows"),
                format!("PERF-OPENVMM-X64-WIN-UEFI-{suffix}.json")
            );
            assert_eq!(
                profile.file_for("aarch64", "windows"),
                format!("PERF-OPENVMM-ARM64-WIN-UEFI-{suffix}.json")
            );
        }
    }
}
