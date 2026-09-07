// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context as _;
use std::collections::VecDeque;
use std::path::Path;

pub(crate) struct RunDiagnostics<'a> {
    pub(crate) config_output_dir: &'a Path,
    pub(crate) virtual_client_logs: &'a Path,
    pub(crate) profile_work_dir: &'a Path,
    pub(crate) temp_dir: &'a Path,
    pub(crate) runtime_logs: &'a Path,
}

impl RunDiagnostics<'_> {
    pub(crate) fn collect(&self, process_success: bool) -> Vec<String> {
        let mut errors = Vec::new();
        if let Err(err) = copy_profile_diagnostics(
            [("data", self.profile_work_dir), ("temp", self.temp_dir)],
            self.config_output_dir,
        ) {
            errors.push(format!("failed to copy profile diagnostics: {err:#}"));
        }
        if let Err(err) = copy_virtual_client_results(
            [
                ("logs", self.virtual_client_logs),
                ("runtime", self.runtime_logs),
            ],
            &self.config_output_dir.join("virtual-client"),
        ) {
            errors.push(format!("failed to copy VirtualClient results: {err:#}"));
        }

        let metrics_path = self.virtual_client_logs.join("vc.metrics");
        if process_success && !metrics_path.is_file() {
            errors.push(format!(
                "VirtualClient metrics file does not exist: {}",
                metrics_path.display()
            ));
        }
        errors
    }
}

fn copy_profile_diagnostics<'a>(
    sources: impl IntoIterator<Item = (&'a str, &'a Path)>,
    output_dir: &Path,
) -> anyhow::Result<()> {
    for (source_name, source) in sources {
        let mut pending = VecDeque::from([source.to_path_buf()]);
        while let Some(directory) = pending.pop_front() {
            if !directory.exists() {
                continue;
            }
            for entry in fs_err::read_dir(&directory)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    pending.push_back(path);
                    continue;
                }
                let Some(extension) = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| extension.to_ascii_lowercase())
                else {
                    continue;
                };
                let category = match (
                    path.file_name().and_then(|name| name.to_str()),
                    extension.as_str(),
                ) {
                    (Some("metrics.csv"), _) => "results",
                    (_, "log") => "openvmm-logs",
                    _ => continue,
                };
                let relative = path.strip_prefix(source)?;
                let destination = output_dir.join(category).join(source_name).join(relative);
                fs_err::create_dir_all(
                    destination
                        .parent()
                        .context("diagnostic destination has no parent")?,
                )?;
                fs_err::copy(path, destination)?;
            }
        }
    }
    Ok(())
}

fn copy_virtual_client_results<'a>(
    sources: impl IntoIterator<Item = (&'a str, &'a Path)>,
    output_dir: &Path,
) -> anyhow::Result<()> {
    for (source_name, source) in sources {
        let mut pending = VecDeque::from([source.to_path_buf()]);
        while let Some(directory) = pending.pop_front() {
            if !directory.exists() {
                continue;
            }
            for entry in fs_err::read_dir(&directory)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    pending.push_back(path);
                    continue;
                }
                let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !is_publishable_virtual_client_file(filename) {
                    continue;
                }
                let relative = path.strip_prefix(source)?;
                let destination = output_dir.join(source_name).join(relative);
                fs_err::create_dir_all(
                    destination
                        .parent()
                        .context("VirtualClient result destination has no parent")?,
                )?;
                fs_err::copy(path, destination)?;
            }
        }
    }
    Ok(())
}

fn is_publishable_virtual_client_file(filename: &str) -> bool {
    filename == "console.log"
        || filename == "metrics.csv"
        || filename == "vc.metrics"
        || filename == "vc.traces"
        || filename
            .strip_prefix("vc_")
            .is_some_and(|name| name.ends_with(".metrics"))
}

#[cfg(test)]
fn collect_relative_files(root: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut pending = VecDeque::from([root.to_path_buf()]);
    while let Some(directory) = pending.pop_front() {
        if !directory.exists() {
            continue;
        }
        for entry in fs_err::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push_back(entry.path());
            } else {
                files.push(entry.path().strip_prefix(root)?.to_path_buf());
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
fn write_file(path: &Path) -> anyhow::Result<()> {
    fs_err::create_dir_all(path.parent().context("test path has no parent")?)?;
    fs_err::write(path, "test")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RunDiagnostics;
    use super::collect_relative_files;
    use super::write_file;
    use crate::test_support;
    use std::path::PathBuf;
    use test_with_tracing::test;

    #[test]
    fn collects_publishable_result_paths() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("diagnostics")?;
        let config_output_dir = scratch.path().join("out");
        let virtual_client_logs = scratch.path().join("virtual-client");
        let profile_work_dir = scratch.path().join("data");
        let temp_dir = scratch.path().join("temp");
        let runtime_logs = scratch.path().join("runtime");

        fs_err::create_dir_all(&virtual_client_logs)?;
        write_file(&virtual_client_logs.join("vc.metrics"))?;
        write_file(&virtual_client_logs.join("vc_1.metrics"))?;
        write_file(&virtual_client_logs.join("vc.traces"))?;
        write_file(&virtual_client_logs.join("metrics.csv"))?;
        write_file(&virtual_client_logs.join("console.log"))?;
        write_file(&profile_work_dir.join("nested").join("metrics.csv"))?;
        write_file(&profile_work_dir.join("nested").join("result.json"))?;
        write_file(&profile_work_dir.join("nested").join("guest.log"))?;
        write_file(&temp_dir.join("nested").join("samples.csv"))?;
        write_file(&runtime_logs.join("nested").join("vc_runtime.metrics"))?;
        write_file(&runtime_logs.join("nested").join("runtime.log"))?;

        let errors = RunDiagnostics {
            config_output_dir: &config_output_dir,
            virtual_client_logs: &virtual_client_logs,
            profile_work_dir: &profile_work_dir,
            temp_dir: &temp_dir,
            runtime_logs: &runtime_logs,
        }
        .collect(true);

        assert!(errors.is_empty(), "{errors:?}");
        let path = |components: &[&str]| {
            components
                .iter()
                .fold(PathBuf::new(), |path, component| path.join(component))
        };
        assert_eq!(
            collect_relative_files(&config_output_dir)?,
            Vec::from([
                path(&["openvmm-logs", "data", "nested", "guest.log"]),
                path(&["results", "data", "nested", "metrics.csv"]),
                path(&["virtual-client", "logs", "console.log"]),
                path(&["virtual-client", "logs", "metrics.csv"]),
                path(&["virtual-client", "logs", "vc.metrics"]),
                path(&["virtual-client", "logs", "vc.traces"]),
                path(&["virtual-client", "logs", "vc_1.metrics"]),
                path(&["virtual-client", "runtime", "nested", "vc_runtime.metrics",]),
            ])
        );
        Ok(())
    }

    #[test]
    fn reports_missing_metrics_for_successful_runs() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("diagnostics-metrics")?;
        let config_output_dir = scratch.path().join("out");
        let virtual_client_logs = config_output_dir.join("virtual-client");
        let runtime_logs = scratch.path().join("runtime");

        fs_err::create_dir_all(&virtual_client_logs)?;
        fs_err::create_dir_all(&runtime_logs)?;

        let errors = RunDiagnostics {
            config_output_dir: &config_output_dir,
            virtual_client_logs: &virtual_client_logs,
            profile_work_dir: scratch.path(),
            temp_dir: scratch.path(),
            runtime_logs: &runtime_logs,
        }
        .collect(true);

        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("vc.metrics"));
        Ok(())
    }
}
