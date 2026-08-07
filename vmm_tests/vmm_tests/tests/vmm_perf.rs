// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMM.Perf profiles executed through the Petri/nextest test harness.

#![forbid(unsafe_code)]

// xtask-fmt allow-target-arch oneoff-petri-native-test-deps
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod vmm_perf {
    use anyhow::Context as _;
    use petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY;
    use petri_artifacts_vmm_test::artifacts;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Instant;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    const ITERATIONS: u32 = 1;

    struct Profile {
        name: &'static str,
        file: &'static str,
    }

    const FIO: Profile = Profile {
        name: "fio",
        file: "PERF-OPENVMM-FIO.json",
    };
    const IPERF3: Profile = Profile {
        name: "iperf3",
        file: "PERF-OPENVMM-IPERF3.json",
    };
    const BOOT_TIME: Profile = Profile {
        name: "boot_time",
        file: "PERF-OPENVMM-BOOTTIME.json",
    };

    struct VmmPerfArtifacts {
        openvmm: petri::ResolvedArtifact,
        firmware: petri::ResolvedArtifact,
        runtime_archive: petri::ResolvedArtifact,
        log_dir: petri::ResolvedArtifact,
    }

    fn resolve_vmm_perf(resolver: &petri::ArtifactResolver<'_>) -> Option<VmmPerfArtifacts> {
        Some(VmmPerfArtifacts {
            openvmm: resolver.require(artifacts::OPENVMM_NATIVE).erase(),
            firmware: resolver
                .require(artifacts::loadable::UEFI_FIRMWARE_X64)
                .erase(),
            runtime_archive: resolver
                .require(artifacts::vmm_perf::RUNTIME_NATIVE)
                .erase(),
            log_dir: resolver.require(TEST_LOG_DIRECTORY).erase(),
        })
    }

    fn run_fio(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        run_profile(params, artifacts, FIO)
    }

    fn run_iperf3(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        run_profile(params, artifacts, IPERF3)
    }

    fn run_boot_time(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
    ) -> anyhow::Result<()> {
        run_profile(params, artifacts, BOOT_TIME)
    }

    fn run_profile(
        params: petri::PetriTestParams<'_>,
        artifacts: VmmPerfArtifacts,
        profile: Profile,
    ) -> anyhow::Result<()> {
        validate_host()?;

        let openvmm = artifacts.openvmm.get();
        let firmware = artifacts.firmware.get();
        let runtime_archive = artifacts.runtime_archive.get();
        let output_dir = artifacts.log_dir.get();
        ensure_file(openvmm, "OpenVMM executable")?;
        ensure_file(firmware, "MSVM firmware")?;
        ensure_file(runtime_archive, "VMM.Perf runtime archive")?;
        fs_err::create_dir_all(output_dir)?;

        let virtual_client_name = "VirtualClient";
        let runtime_dir = prepare_runtime(runtime_archive, virtual_client_name)?;
        register_package_file(&runtime_dir, "openvmm", "openvmm", openvmm)?;
        register_package_file(
            &runtime_dir,
            "msvm-firmware",
            Path::new("FV").join("MSVM.fd"),
            firmware,
        )?;
        ensure_runtime_executables(&runtime_dir, virtual_client_name)?;

        let profile_path = runtime_dir.join("profiles").join(profile.file);
        ensure_file(&profile_path, "VMM.Perf profile")?;

        let work_parent = std::env::var_os("VMM_PERF_WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs_err::create_dir_all(&work_parent)?;
        let work = tempfile::Builder::new()
            .prefix(&format!("vmm-perf-{}-", profile.name))
            .tempdir_in(work_parent)?;
        let data_dir = work.path().join("data");
        let temp_dir = work.path().join("temp");
        let control_dir = work.path().join("control");
        fs_err::create_dir_all(&data_dir)?;
        fs_err::create_dir_all(&temp_dir)?;
        fs_err::create_dir_all(&control_dir)?;

        let patched_profile = control_dir.join(profile.file);
        patch_profile_work_dir(&profile_path, &patched_profile, &data_dir)?;

        let virtual_client_logs = output_dir.join("virtual-client");
        let results_dir = output_dir.join("results");
        let openvmm_logs_dir = output_dir.join("openvmm-logs");
        fs_err::create_dir_all(&virtual_client_logs)?;
        fs_err::create_dir_all(&results_dir)?;
        fs_err::create_dir_all(&openvmm_logs_dir)?;

        let runtime_logs = runtime_dir.join("logs");
        if runtime_logs.exists() {
            fs_err::remove_dir_all(&runtime_logs)?;
        }

        let experiment_id = format!(
            "{}-{}",
            profile.name,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        );
        let console_log_path = output_dir.join("console.log");
        let console_log = File::create(&console_log_path)?;
        let started = Instant::now();
        let status = run_virtual_client(
            &runtime_dir,
            virtual_client_name,
            &patched_profile,
            &data_dir,
            &temp_dir,
            &virtual_client_logs,
            &experiment_id,
            console_log,
        )?;
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        copy_profile_diagnostics(
            [("data", data_dir.as_path()), ("temp", temp_dir.as_path())],
            output_dir,
        )?;
        if runtime_logs.exists() {
            copy_directory(&runtime_logs, &virtual_client_logs.join("runtime"))?;
        }

        let exit_code = status.code().unwrap_or(-1);
        fs_err::write(
            output_dir.join("run-summary.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "profile": profile.file,
                "success": status.success(),
                "exit_code": exit_code,
                "experiment_id": experiment_id,
                "duration_ms": duration_ms,
                "runtime_rid": "linux-x64",
                "runtime_version": "3.0.21",
                "runtime_source": "public-blob",
            }))?,
        )?;

        tracing::info!(
            test = params.test_name,
            profile = profile.file,
            exit_code,
            duration_ms,
            "VMM.Perf profile completed"
        );

        anyhow::ensure!(
            status.success(),
            "VMM.Perf profile {} failed with exit code {}",
            profile.file,
            exit_code
        );
        Ok(())
    }

    fn validate_host() -> anyhow::Result<()> {
        anyhow::ensure!(
            Path::new("/dev/kvm").exists(),
            "Linux VMM.Perf profiles require /dev/kvm"
        );
        if !running_as_root()? {
            let status = Command::new("sudo")
                .args(["-n", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to check passwordless sudo")?;
            anyhow::ensure!(
                status.success(),
                "Linux VMM.Perf profiles require passwordless sudo"
            );
        }

        Ok(())
    }

    fn run_virtual_client(
        runtime_dir: &Path,
        virtual_client_name: &str,
        profile: &Path,
        data_dir: &Path,
        temp_dir: &Path,
        log_dir: &Path,
        experiment_id: &str,
        console_log: File,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let virtual_client = runtime_dir.join(virtual_client_name);
        let package_dir = runtime_dir.join("packages");
        let running_as_root = running_as_root()?;
        let owner = if running_as_root {
            None
        } else {
            Some(current_user_and_group()?)
        };
        let mut command;

        if running_as_root {
            command = Command::new(&virtual_client);
            command
                .env("VC_VMM_WORK_DIR", data_dir)
                .env("TEMP", temp_dir)
                .env("TMP", temp_dir)
                .env("TMPDIR", temp_dir);
        } else {
            command = Command::new("sudo");
            command
                .args(["-n", "env"])
                .arg(format!("VC_VMM_WORK_DIR={}", data_dir.display()))
                .arg(format!("TEMP={}", temp_dir.display()))
                .arg(format!("TMP={}", temp_dir.display()))
                .arg(format!("TMPDIR={}", temp_dir.display()))
                .arg(&virtual_client);
        }

        let stderr = console_log.try_clone()?;
        let status = command
            .current_dir(runtime_dir)
            .arg(format!("--profile={}", profile.display()))
            .arg(format!("--iterations={ITERATIONS}"))
            .arg(format!("--package-dir={}", package_dir.display()))
            .arg(format!("--log-dir={}", log_dir.display()))
            .arg(format!("--experiment-id={experiment_id}"))
            .arg("--logger=csv")
            .arg("--logger=summary")
            .arg("--log-to-file")
            .stdout(Stdio::from(console_log))
            .stderr(Stdio::from(stderr))
            .status()
            .with_context(|| {
                format!(
                    "failed to launch VMM.Perf VirtualClient {}",
                    virtual_client.display()
                )
            });

        let ownership = if let Some((uid, gid)) = owner {
            restore_ownership(&uid, &gid, &[runtime_dir, data_dir, temp_dir, log_dir])
        } else {
            Ok(())
        };

        match (status, ownership) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(err),
            (Err(run_err), Err(ownership_err)) => Err(run_err.context(format!(
                "failed to restore VMM.Perf file ownership after execution: {ownership_err:#}"
            ))),
        }
    }

    fn running_as_root() -> anyhow::Result<bool> {
        Ok(current_id("-u", "user")? == "0")
    }

    fn current_user_and_group() -> anyhow::Result<(String, String)> {
        Ok((current_id("-u", "user")?, current_id("-g", "group")?))
    }

    fn current_id(flag: &str, description: &str) -> anyhow::Result<String> {
        let output = Command::new("id")
            .arg(flag)
            .output()
            .with_context(|| format!("failed to query the current {description} ID"))?;
        anyhow::ensure!(output.status.success(), "`id {flag}` failed");
        let id = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()),
            "`id {flag}` returned an invalid {description} ID"
        );
        Ok(id)
    }

    fn restore_ownership(uid: &str, gid: &str, paths: &[&Path]) -> anyhow::Result<()> {
        let status = Command::new("sudo")
            .args(["-n", "chown", "-R", "--"])
            .arg(format!("{uid}:{gid}"))
            .args(paths)
            .status()
            .context("failed to launch ownership restoration for VMM.Perf files")?;
        anyhow::ensure!(
            status.success(),
            "failed to restore VMM.Perf file ownership to {uid}:{gid}"
        );
        Ok(())
    }

    fn prepare_runtime(archive: &Path, virtual_client_name: &str) -> anyhow::Result<PathBuf> {
        let archive_parent = archive
            .parent()
            .context("VMM.Perf archive has no parent directory")?;
        let cache_dir = archive_parent.join("vmm-perf-runtime-linux-x64-1.0.0");

        if let Ok(runtime_dir) = find_runtime_dir(&cache_dir, virtual_client_name) {
            return Ok(runtime_dir);
        }
        if cache_dir.exists() {
            fs_err::remove_dir_all(&cache_dir)?;
        }

        let staging = archive_parent.join(format!(
            ".vmm-perf-runtime-extracting-{}",
            std::process::id()
        ));
        if staging.exists() {
            fs_err::remove_dir_all(&staging)?;
        }
        fs_err::create_dir_all(&staging)?;

        let status = Command::new("tar")
            .args(["-xf"])
            .arg(archive)
            .arg("-C")
            .arg(&staging)
            .status()
            .context("failed to launch tar for VMM.Perf runtime extraction")?;
        anyhow::ensure!(status.success(), "failed to extract VMM.Perf runtime");
        find_runtime_dir(&staging, virtual_client_name)?;

        match fs_err::rename(&staging, &cache_dir) {
            Ok(()) => {}
            Err(err) if cache_dir.exists() => {
                fs_err::remove_dir_all(&staging)?;
                find_runtime_dir(&cache_dir, virtual_client_name)
                    .context("concurrent VMM.Perf runtime extraction produced an invalid cache")?;
                tracing::debug!(%err, "using concurrently extracted VMM.Perf runtime");
            }
            Err(err) => return Err(err.into()),
        }

        find_runtime_dir(&cache_dir, virtual_client_name)
    }

    fn find_runtime_dir(root: &Path, virtual_client_name: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(root.is_dir(), "runtime extraction directory is missing");
        let mut pending = VecDeque::from([(root.to_path_buf(), 0_u8)]);
        let mut candidates = Vec::new();

        while let Some((directory, depth)) = pending.pop_front() {
            if directory.join(virtual_client_name).is_file() {
                candidates.push(directory.clone());
            }
            if depth == 4 {
                continue;
            }
            for entry in fs_err::read_dir(&directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push_back((entry.path(), depth + 1));
                }
            }
        }

        match candidates.as_slice() {
            [runtime_dir] => Ok(runtime_dir.clone()),
            [] => anyhow::bail!(
                "VMM.Perf archive did not contain {virtual_client_name} within four directory levels"
            ),
            _ => anyhow::bail!("VMM.Perf archive contained multiple runtime directories"),
        }
    }

    fn patch_profile_work_dir(
        source: &Path,
        destination: &Path,
        work_dir: &Path,
    ) -> anyhow::Result<()> {
        let mut profile: serde_json::Value = serde_json::from_slice(&fs_err::read(source)?)?;
        let actions = profile
            .get_mut("Actions")
            .and_then(serde_json::Value::as_array_mut)
            .with_context(|| format!("profile {} has no Actions array", source.display()))?;

        let mut patched = false;
        for action in actions {
            let Some(parameters) = action
                .get_mut("Parameters")
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            let Some(vmm) = parameters.get("Vmm").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if vmm != "OpenVMM" {
                continue;
            }
            parameters.insert(
                "Vmm.MemoryBackingDirectory".into(),
                serde_json::Value::String(work_dir.display().to_string()),
            );
            patched = true;
        }

        anyhow::ensure!(
            patched,
            "profile {} did not contain a supported VMM action",
            source.display()
        );
        fs_err::write(destination, serde_json::to_vec_pretty(&profile)?)?;
        Ok(())
    }

    fn register_package_file(
        runtime_dir: &Path,
        package_name: &str,
        relative_path: impl AsRef<Path>,
        source_path: &Path,
    ) -> anyhow::Result<()> {
        let packages_dir = runtime_dir.join("packages");
        let package_dir = packages_dir.join(package_name);
        let destination = package_dir.join(relative_path);
        fs_err::create_dir_all(
            destination
                .parent()
                .context("package destination has no parent directory")?,
        )?;
        fs_err::copy(source_path, destination)?;
        fs_err::write(
            packages_dir.join(format!("{package_name}.vcpkgreg")),
            serde_json::to_vec(&serde_json::json!({
                "name": package_name,
                "path": package_dir,
                "timestamp": "1970-01-01T00:00:00Z",
                "metadata": {
                    "source": "petri"
                }
            }))?,
        )?;
        Ok(())
    }

    fn ensure_runtime_executables(
        runtime_dir: &Path,
        virtual_client_name: &str,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        for path in [
            runtime_dir.join(virtual_client_name),
            runtime_dir.join("cidata-inject"),
            runtime_dir.join("packages").join("openvmm").join("openvmm"),
        ] {
            if path.is_file() {
                let mut permissions = fs_err::metadata(&path)?.permissions();
                permissions.set_mode(permissions.mode() | 0o111);
                fs_err::set_permissions(path, permissions)?;
            }
        }
        Ok(())
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

    fn ensure_file(path: &Path, description: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            path.is_file(),
            "{description} does not exist or is not a file: {}",
            path.display()
        );
        Ok(())
    }

    petri::multitest!(vec![
        petri::SimpleTest::new("fio", resolve_vmm_perf, run_fio).into(),
        petri::SimpleTest::new("iperf3", resolve_vmm_perf, run_iperf3).into(),
        petri::SimpleTest::new("boot_time", resolve_vmm_perf, run_boot_time).into(),
    ]);
}

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
