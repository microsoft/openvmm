// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-to-guest path and environment mapping for incubator commands.

use anyhow::Context;
use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Maps paths from one host share root to the corresponding guest share root.
#[derive(Debug, Clone)]
pub struct HostPathMapper {
    host_root: PathBuf,
    guest_root: String,
}

impl HostPathMapper {
    /// Creates a mapper from a host share root to a guest share root.
    pub fn new(host_root: impl AsRef<Path>, guest_root: impl Into<String>) -> anyhow::Result<Self> {
        let host_root = host_root.as_ref();
        let host_root = normalize_host_path(host_root).with_context(|| {
            format!(
                "failed to normalize host share root '{}'",
                host_root.display()
            )
        })?;
        let guest_root = normalize_guest_root(guest_root.into())?;

        Ok(Self {
            host_root,
            guest_root,
        })
    }

    /// Maps a host path under the share root to the guest-visible path.
    pub fn map_path(&self, host_path: impl AsRef<Path>) -> anyhow::Result<String> {
        let host_path = host_path.as_ref();
        let normalized = normalize_host_path(host_path)
            .with_context(|| format!("failed to normalize host path '{}'", host_path.display()))?;
        let suffix = normalized.strip_prefix(&self.host_root).with_context(|| {
            format!(
                "host path '{}' is not under shared host root '{}'",
                normalized.display(),
                self.host_root.display()
            )
        })?;

        join_guest_path(&self.guest_root, suffix)
    }
}

/// Applies an `INCUBATOR_ENV` policy to a host environment map.
pub fn guest_env_from_incubator_env(
    policy: &str,
    host_env: &BTreeMap<String, String>,
    path_mapper: &HostPathMapper,
) -> anyhow::Result<BTreeMap<String, String>> {
    IncubatorEnvPolicy::parse(policy)?.apply(host_env, path_mapper)
}

#[derive(Debug, Clone)]
struct IncubatorEnvPolicy {
    entries: Vec<IncubatorEnvEntry>,
}

impl IncubatorEnvPolicy {
    fn parse(policy: &str) -> anyhow::Result<Self> {
        let entries = policy
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(IncubatorEnvEntry::parse)
            .collect::<anyhow::Result<_>>()?;

        Ok(Self { entries })
    }

    fn apply(
        &self,
        host_env: &BTreeMap<String, String>,
        path_mapper: &HostPathMapper,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut guest_env = BTreeMap::new();

        for entry in &self.entries {
            match &entry.selector {
                EnvSelector::Exact(name) => {
                    if let Some(value) = host_env.get(name) {
                        let value = entry
                            .value_mapping
                            .map_value(name, value, path_mapper)
                            .with_context(|| {
                                format!("failed to map INCUBATOR_ENV entry '{name}'")
                            })?;
                        guest_env.insert(name.clone(), value);
                    }
                }
                EnvSelector::Prefix(prefix) => {
                    for (name, value) in host_env {
                        if name.starts_with(prefix) {
                            let value = entry
                                .value_mapping
                                .map_value(name, value, path_mapper)
                                .with_context(|| {
                                    format!("failed to map INCUBATOR_ENV entry '{prefix}*'")
                                })?;
                            guest_env.insert(name.clone(), value);
                        }
                    }
                }
            }
        }

        Ok(guest_env)
    }
}

#[derive(Debug, Clone)]
struct IncubatorEnvEntry {
    selector: EnvSelector,
    value_mapping: EnvValueMapping,
}

impl IncubatorEnvEntry {
    fn parse(entry: &str) -> anyhow::Result<Self> {
        let (name, value_mapping) = match entry.rsplit_once('/') {
            Some((name, "p")) => (name, EnvValueMapping::Path),
            Some((name, "lp")) => (name, EnvValueMapping::PathList),
            Some((_, suffix)) => {
                anyhow::bail!("unknown INCUBATOR_ENV path mapping suffix '/{suffix}' in '{entry}'")
            }
            None => (entry, EnvValueMapping::Unchanged),
        };

        if name.is_empty() {
            anyhow::bail!("INCUBATOR_ENV entry '{entry}' has an empty variable name");
        }
        if name.contains('=') {
            anyhow::bail!("INCUBATOR_ENV entry '{entry}' contains '='");
        }

        let selector = if let Some(prefix) = name.strip_suffix('*') {
            if prefix.is_empty() {
                anyhow::bail!("INCUBATOR_ENV entry '{entry}' has an empty prefix");
            }
            EnvSelector::Prefix(prefix.to_string())
        } else {
            if name.contains('*') {
                anyhow::bail!(
                    "INCUBATOR_ENV entry '{entry}' contains '*' outside the final position"
                );
            }
            EnvSelector::Exact(name.to_string())
        };

        Ok(Self {
            selector,
            value_mapping,
        })
    }
}

#[derive(Debug, Clone)]
enum EnvSelector {
    Exact(String),
    Prefix(String),
}

#[derive(Debug, Clone, Copy)]
enum EnvValueMapping {
    Unchanged,
    Path,
    PathList,
}

impl EnvValueMapping {
    fn map_value(
        self,
        name: &str,
        value: &str,
        path_mapper: &HostPathMapper,
    ) -> anyhow::Result<String> {
        match self {
            EnvValueMapping::Unchanged => Ok(value.to_string()),
            EnvValueMapping::Path => path_mapper
                .map_path(value)
                .with_context(|| format!("environment variable {name} is not in the host share")),
            EnvValueMapping::PathList => map_path_list(name, value, path_mapper),
        }
    }
}

fn map_path_list(name: &str, value: &str, path_mapper: &HostPathMapper) -> anyhow::Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }

    // The incubator targets a Linux guest, so path lists always use the
    // guest's ':' separator regardless of the host OS. Do not use
    // `std::env::split_paths`, which splits on the host separator (';' on
    // Windows) and would mis-parse these values on Windows hosts.
    let mut mapped = Vec::new();
    for entry in value.split(':') {
        let path = Path::new(entry);
        let guest_path = path_mapper.map_path(path).with_context(|| {
            format!(
                "path-list entry '{}' in environment variable {name} is not in the host share",
                path.display()
            )
        })?;
        mapped.push(guest_path);
    }

    Ok(mapped.join(":"))
}

fn normalize_guest_root(mut guest_root: String) -> anyhow::Result<String> {
    if guest_root.is_empty() {
        anyhow::bail!("guest share root must not be empty");
    }
    if !guest_root.starts_with('/') {
        anyhow::bail!("guest share root '{guest_root}' must be absolute");
    }
    while guest_root.len() > 1 && guest_root.ends_with('/') {
        guest_root.pop();
    }
    Ok(guest_root)
}

fn normalize_host_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("path must not be empty");
    }

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to get current directory")?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("path '{}' escapes the filesystem root", path.display());
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }

    Ok(normalized)
}

fn join_guest_path(guest_root: &str, suffix: &Path) -> anyhow::Result<String> {
    let mut guest_path = guest_root.to_string();

    for component in suffix.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => {
                let component = component.to_str().with_context(|| {
                    format!(
                        "mapped host path component '{}' is not valid UTF-8",
                        component.to_string_lossy()
                    )
                })?;
                if guest_path != "/" {
                    guest_path.push('/');
                }
                guest_path.push_str(component);
            }
            _ => anyhow::bail!(
                "mapped path suffix '{}' contains an unexpected component",
                suffix.display()
            ),
        }
    }

    Ok(guest_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GUEST_SHARE_ROOT;

    fn host_env(vars: &[(&str, &str)]) -> BTreeMap<String, String> {
        vars.iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn maps_host_paths_under_share_root() {
        let mapper = HostPathMapper::new("/host/test-content", GUEST_SHARE_ROOT).unwrap();

        assert_eq!(
            mapper
                .map_path("/host/test-content/target/test-bin")
                .unwrap(),
            format!("{GUEST_SHARE_ROOT}/target/test-bin")
        );
        assert_eq!(
            mapper.map_path("/host/test-content").unwrap(),
            GUEST_SHARE_ROOT
        );
    }

    #[test]
    fn rejects_host_paths_outside_share_root() {
        let mapper = HostPathMapper::new("/host/test-content", GUEST_SHARE_ROOT).unwrap();

        let error = mapper.map_path("/host/other/test-bin").unwrap_err();

        assert!(error.to_string().contains("shared host root"));
    }

    #[test]
    fn rejects_parent_component_escape_from_share_root() {
        let mapper = HostPathMapper::new("/host/test-content", GUEST_SHARE_ROOT).unwrap();

        let error = mapper
            .map_path("/host/test-content/../other/test-bin")
            .unwrap_err();

        assert!(error.to_string().contains("shared host root"));
    }

    #[test]
    fn applies_incubator_env_policy() {
        let mapper = HostPathMapper::new("/host/test-content", GUEST_SHARE_ROOT).unwrap();
        let host_env = host_env(&[
            ("RUST_LOG", "debug"),
            ("TEST_OUTPUT_PATH", "/host/test-content/out"),
            (
                "LD_LIBRARY_PATH",
                "/host/test-content/lib:/host/test-content/other-lib",
            ),
            (
                "CARGO_BIN_EXE_vmm_tests",
                "/host/test-content/bin/vmm_tests",
            ),
            ("CARGO_BIN_OTHER", "/host/test-content/bin/ignored"),
            ("NEXTEST_BIN_EXE_helper", "/host/test-content/bin/helper"),
        ]);

        let guest_env = guest_env_from_incubator_env(
            "RUST_LOG:RUST_BACKTRACE:TEST_OUTPUT_PATH/p:LD_LIBRARY_PATH/lp:CARGO_BIN_EXE_*/p:NEXTEST_BIN_EXE_*/p",
            &host_env,
            &mapper,
        )
        .unwrap();

        assert_eq!(guest_env.get("RUST_LOG").unwrap(), "debug");
        assert!(!guest_env.contains_key("RUST_BACKTRACE"));
        assert_eq!(
            guest_env.get("TEST_OUTPUT_PATH").unwrap(),
            &format!("{GUEST_SHARE_ROOT}/out")
        );
        assert_eq!(
            guest_env.get("LD_LIBRARY_PATH").unwrap(),
            &format!("{GUEST_SHARE_ROOT}/lib:{GUEST_SHARE_ROOT}/other-lib")
        );
        assert_eq!(
            guest_env.get("CARGO_BIN_EXE_vmm_tests").unwrap(),
            &format!("{GUEST_SHARE_ROOT}/bin/vmm_tests")
        );
        assert!(!guest_env.contains_key("CARGO_BIN_OTHER"));
        assert_eq!(
            guest_env.get("NEXTEST_BIN_EXE_helper").unwrap(),
            &format!("{GUEST_SHARE_ROOT}/bin/helper")
        );
    }

    #[test]
    fn errors_when_path_env_is_outside_share_root() {
        let mapper = HostPathMapper::new("/host/test-content", GUEST_SHARE_ROOT).unwrap();
        let host_env = host_env(&[("TEST_OUTPUT_PATH", "/host/other/out")]);

        let error =
            guest_env_from_incubator_env("TEST_OUTPUT_PATH/p", &host_env, &mapper).unwrap_err();

        assert!(error.to_string().contains("TEST_OUTPUT_PATH"));
    }

    #[test]
    fn rejects_invalid_policy_entry() {
        let error = IncubatorEnvPolicy::parse("CARGO_*_EXE/p").unwrap_err();

        assert!(error.to_string().contains("outside the final position"));
    }
}
