// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::host::HostCapacity;
use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const CONFIGS_ENV: &str = "VMM_PERF_VM_SIZES";
const PARAMETERS_ENV: &str = "VMM_PERF_PARAMETERS";
const MIB: u64 = 1024 * 1024;
const MIB_PER_GIB: u64 = 1024;
const HIGH_CAPACITY_PERCENT: u64 = 80;

#[derive(Clone, Copy, Debug)]
pub(crate) enum VmmPerfProfile {
    Fio,
    Iperf3,
    BootTime,
}

impl VmmPerfProfile {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fio => "fio",
            Self::Iperf3 => "iperf3",
            Self::BootTime => "boot_time",
        }
    }

    pub(crate) fn file(self) -> &'static str {
        if cfg!(target_os = "windows") {
            match self {
                Self::Fio => "PERF-OPENVMM-WHP-FIO.json",
                Self::Iperf3 => "PERF-OPENVMM-WHP-IPERF3.json",
                Self::BootTime => "PERF-OPENVMM-WHP-BOOTTIME.json",
            }
        } else {
            match self {
                Self::Fio => "PERF-OPENVMM-FIO.json",
                Self::Iperf3 => "PERF-OPENVMM-IPERF3.json",
                Self::BootTime => "PERF-OPENVMM-BOOTTIME.json",
            }
        }
    }
}

#[derive(Debug)]
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

pub(crate) fn selected_configs(capacity: HostCapacity) -> anyhow::Result<Vec<VmmPerfConfig>> {
    let raw_configs = read_json_env::<Vec<RawConfig>>(CONFIGS_ENV)?.unwrap_or_default();
    let mut configs = if raw_configs.is_empty() {
        default_configs(capacity)?
    } else {
        configs_from_raw(raw_configs)?
    };

    if let Some(parameters) = read_json_env::<BTreeMap<String, serde_json::Value>>(PARAMETERS_ENV)?
    {
        apply_parameters(
            &mut configs,
            stringify_parameters(parameters, PARAMETERS_ENV)?,
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

fn read_json_env<T: DeserializeOwned>(name: &str) -> anyhow::Result<Option<T>> {
    let Some(json) = std::env::var_os(name) else {
        return Ok(None);
    };
    let json = json
        .into_string()
        .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8"))?;
    serde_json::from_str(&json)
        .with_context(|| format!("failed to parse {name}"))
        .map(Some)
}

fn stringify_parameters(
    parameters: BTreeMap<String, serde_json::Value>,
    context: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    parameters
        .into_iter()
        .map(|(name, value)| {
            anyhow::ensure!(
                !name.trim().is_empty(),
                "{context} contains an empty parameter name"
            );
            Ok((name.clone(), scalar_to_string(&name, &value)?))
        })
        .collect()
}

fn default_configs(capacity: HostCapacity) -> anyhow::Result<Vec<VmmPerfConfig>> {
    let high_cpu_count = capacity
        .logical_processors
        .saturating_mul(HIGH_CAPACITY_PERCENT as usize)
        / 100;
    let high_cpu_count =
        u64::try_from(high_cpu_count.max(1)).context("host processor count is too large")?;
    let high_memory_mb = capacity
        .available_memory_bytes
        .saturating_mul(HIGH_CAPACITY_PERCENT)
        / 100
        / MIB
        / MIB_PER_GIB
        * MIB_PER_GIB;

    Ok([
        (4, 8 * MIB_PER_GIB),
        (8, 16 * MIB_PER_GIB),
        (high_cpu_count, high_memory_mb.max(MIB_PER_GIB)),
    ]
    .into_iter()
    .map(|(cpu_count, memory_mb)| VmmPerfConfig {
        name: shape_name(cpu_count, memory_mb),
        parameters: BTreeMap::from([
            ("CpuCount".into(), cpu_count.to_string()),
            ("MemoryMB".into(), memory_mb.to_string()),
        ]),
        name_is_explicit: false,
    })
    .collect())
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
