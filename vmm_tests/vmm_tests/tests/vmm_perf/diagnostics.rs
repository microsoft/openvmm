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
        if self.runtime_logs.exists()
            && let Err(err) =
                copy_directory(self.runtime_logs, &self.virtual_client_logs.join("runtime"))
        {
            errors.push(format!("failed to copy runtime logs: {err:#}"));
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
                let category = match extension.as_str() {
                    "csv" | "json" => "results",
                    "log" => "openvmm-logs",
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

fn copy_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let mut pending = VecDeque::from([(source.to_path_buf(), destination.to_path_buf())]);
    while let Some((source, destination)) = pending.pop_front() {
        fs_err::create_dir_all(&destination)?;
        for entry in fs_err::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                pending.push_back((entry.path(), target));
            } else {
                fs_err::copy(entry.path(), target)?;
            }
        }
    }
    Ok(())
}
