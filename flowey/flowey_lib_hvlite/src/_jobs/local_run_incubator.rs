// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Prepare a standalone mshv Incubator using reusable build/acquisition nodes.

use crate::common::CommonArch;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use sha2::Digest as _;

flowey_request! {
    pub struct Params {
        pub profile: CommonProfile,
        pub kernel: Option<PathBuf>,
        pub initrd: Option<PathBuf>,
        pub firmware: Option<PathBuf>,
        pub boot_artifacts: Option<PathBuf>,
        pub rpm_cache: Option<PathBuf>,
        pub offline: bool,
        pub share: PathBuf,
        pub dir: PathBuf,
        pub kernel_cmdline: Option<String>,
        pub hypervisor_cmdline: Option<String>,
        pub prepare_only: bool,
        pub timeout: u64,
        pub command: Vec<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_mshv_host_boot::Node>();
        ctx.import::<crate::resolve_mshv_boot_artifacts::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            profile,
            kernel,
            initrd,
            firmware,
            boot_artifacts,
            rpm_cache,
            offline,
            share,
            dir,
            kernel_cmdline,
            hypervisor_cmdline,
            prepare_only,
            timeout,
            command,
            done,
        } = request;
        ctx.config(crate::resolve_mshv_boot_artifacts::Config {
            local_path: boot_artifacts.map(|path| ConfigVar(ReadVar::from_static(path))),
            cache_dir: rpm_cache.map(|path| ConfigVar(ReadVar::from_static(path))),
            offline: Some(offline),
        });
        let repo = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let openvmm = ctx.reqv(|openvmm| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                target: CommonTriple::X86_64_LINUX_GNU,
                profile,
                features: Default::default(),
            },
            openvmm,
        });
        let incubator = ctx.reqv(|incubator| crate::build_incubator::Request {
            target: CommonTriple::X86_64_LINUX_GNU,
            profile: if profile.to_release() {
                crate::run_cargo_build::BuildProfile::Release
            } else {
                crate::run_cargo_build::BuildProfile::Light
            },
            incubator,
        });
        let pipette = ctx.reqv(|pipette| crate::build_pipette::Request {
            target: CommonTriple::X86_64_LINUX_MUSL,
            profile,
            pipette,
        });
        let host_boot =
            ctx.reqv(|host_boot| crate::build_mshv_host_boot::Request { profile, host_boot });
        let artifacts = ctx.reqv(|artifacts| crate::resolve_mshv_boot_artifacts::Request {
            arch: CommonArch::X86_64,
            artifacts,
        });
        let kernel = kernel.map(ReadVar::from_static).unwrap_or_else(|| {
            ctx.reqv(|kernel| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::BzImage,
                    CommonArch::X86_64,
                    crate::resolve_openvmm_test_linux_kernel::LinuxTestKernelVersion::MshvTest,
                    kernel,
                )
            })
        });
        let initrd = initrd.map(ReadVar::from_static).unwrap_or_else(|| {
            ctx.reqv(|initrd| {
                crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::X86_64, initrd)
            })
        });
        let firmware = firmware.map(ReadVar::from_static).unwrap_or_else(|| {
            ctx.reqv(|msvm_fd| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
                arch: CommonArch::X86_64,
                flavor: None,
                msvm_fd,
            })
        });

        ctx.emit_rust_step("prepare standalone Incubator profile and command", |ctx| {
            let repo = repo.claim(ctx);
            let openvmm = openvmm.claim(ctx);
            let incubator = incubator.claim(ctx);
            let pipette = pipette.claim(ctx);
            let host_boot = host_boot.claim(ctx);
            let artifacts = artifacts.claim(ctx);
            let kernel = kernel.claim(ctx);
            let initrd = initrd.claim(ctx);
            let firmware = firmware.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let repo = rt.read(repo);
                let runner = rt.read(incubator).bin;
                let crate::build_openvmm::OpenvmmOutput::LinuxBin { bin: openvmm, .. } =
                    rt.read(openvmm)
                else {
                    unreachable!()
                };
                let crate::build_pipette::PipetteOutput::LinuxBin { bin: pipette, .. } =
                    rt.read(pipette)
                else {
                    unreachable!()
                };
                let host_boot = rt.read(host_boot).efi;
                let artifacts = rt.read(artifacts);
                let kernel = rt.read(kernel);
                let firmware = rt.read(firmware);
                let initrd = rt.read(initrd);
                fs_err::create_dir_all(&dir)?;
                let runtime = stage_runtime(
                    &dir.join("runtime"),
                    &[
                        ("openvmm", &openvmm),
                        ("incubator", &runner),
                        ("pipette", &pipette),
                        ("mshv_host_boot.efi", &host_boot),
                    ],
                )?;
                let openvmm = runtime.join("openvmm");
                let runner = runtime.join("incubator");
                let host_boot = runtime.join("mshv_host_boot.efi");
                let pipette_path = runtime.join("pipette");
                let relative_pipette = pipette_path.strip_prefix(&share)?;
                let guest_pipette = format!("/share/{}", path_str(relative_pipette)?);

                let template = crate::build_incubator::incubator_profile_path(&repo, "x86_64-mshv");
                let mut config: toml_edit::DocumentMut =
                    fs_err::read_to_string(template)?.parse()?;
                config["incubator"]["binary"] = toml_edit::value(path_str(&openvmm)?);
                config["boot"]["host-boot"] = toml_edit::value(path_str(&host_boot)?);
                config["boot"]["firmware"] = toml_edit::value(path_str(&firmware)?);
                config["boot"]["boot-artifacts"] = toml_edit::value(path_str(&artifacts)?);
                if let Some(value) = kernel_cmdline {
                    config["boot"]["kernel-cmdline"] = toml_edit::value(value);
                }
                if let Some(value) = hypervisor_cmdline {
                    config["boot"]["hypervisor-cmdline"] = toml_edit::value(value);
                }
                let profile_path = dir.join("profile.toml");
                let profile_file = tempfile::NamedTempFile::new_in(&dir)?;
                fs_err::write(profile_file.path(), config.to_string())?;
                profile_file.persist(&profile_path)?;
                let args = vec![
                    "--profile".into(),
                    path_str(&profile_path)?.into(),
                    "--kernel".into(),
                    path_str(&kernel)?.into(),
                    "--initrd".into(),
                    path_str(&initrd)?.into(),
                    "--share".into(),
                    path_str(&share)?.into(),
                    "--guest-pipette".into(),
                    guest_pipette,
                    "--output-dir".into(),
                    path_str(&dir.join("results"))?.into(),
                    "--timeout".into(),
                    timeout.to_string(),
                ];
                log::info!("Prepared profile: {}", profile_path.display());
                if prepare_only {
                    let command = std::iter::once(path_str(&runner)?)
                        .chain(args.iter().map(String::as_str))
                        .chain(std::iter::once("/bin/sh"))
                        .map(shell_word)
                        .collect::<Vec<_>>()
                        .join(" ");
                    log::info!("Run directly: {command}");
                } else {
                    flowey::shell_cmd!(rt, "{runner}")
                        .args(args)
                        .args(["--no-pty", "--"])
                        .args(command)
                        .run()?;
                }
                Ok(())
            }
        });
        Ok(())
    }
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn shell_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

// Build outputs live in Flowey's replaceable work directory. Keep an immutable
// pair for direct runs, without copying unchanged binaries on each kernel edit.
fn stage_runtime(root: &Path, files: &[(&str, &Path)]) -> anyhow::Result<PathBuf> {
    let mut hash = sha2::Sha256::new();
    let mut hashes = Vec::new();
    for (name, source) in files {
        let digest = sha2::Sha256::digest(fs_err::read(source)?);
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(digest);
        hashes.push((*name, digest));
    }
    let key = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs_err::create_dir_all(root)?;
    let output = root.join(key);
    if !output.exists() {
        let scratch = tempfile::Builder::new()
            .prefix(".prepare-")
            .tempdir_in(root)?;
        for (name, source) in files {
            fs_err::copy(source, scratch.path().join(name))?;
        }
        match fs_err::rename(scratch.path(), &output) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                log::debug!("another writer staged runtime {}", output.display());
            }
            Err(error) => return Err(error.into()),
        }
    }
    for (name, digest) in hashes {
        let path = output.join(name);
        anyhow::ensure!(
            fs_err::symlink_metadata(&path)?.file_type().is_file()
                && sha2::Sha256::digest(fs_err::read(&path)?) == digest,
            "staged Incubator runtime was modified: {}",
            path.display()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn retains_coherent_runtime_snapshots_across_builds() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let source = scratch.path().join("ephemeral-build");
        let cache = scratch.path().join("runtime");
        fs_err::write(&source, b"old build")?;
        let old = stage_runtime(&cache, &[("incubator", &source)])?;
        assert_eq!(stage_runtime(&cache, &[("incubator", &source)])?, old);
        fs_err::write(&source, b"new build")?;
        let new = stage_runtime(&cache, &[("incubator", &source)])?;
        assert_ne!(new, old);
        fs_err::remove_file(&source)?;
        assert_eq!(fs_err::read(old.join("incubator"))?, b"old build");
        assert_eq!(fs_err::read(new.join("incubator"))?, b"new build");
        fs_err::write(&source, b"new build")?;
        fs_err::write(new.join("incubator"), b"corrupt")?;
        assert!(stage_runtime(&cache, &[("incubator", &source)]).is_err());
        Ok(())
    }
}
