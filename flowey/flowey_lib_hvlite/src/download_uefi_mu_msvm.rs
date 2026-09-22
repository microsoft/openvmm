// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built mu_msvm package from its GitHub Release.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_config! {
    /// Config for the download_uefi_mu_msvm node.
    pub struct Config {
        /// Specify version of mu_msvm to use
        pub version: Option<String>,
        /// Use a local MSVM.fd path, keyed by architecture
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Download the mu_msvm package for the given arch
        GetMsvmFd {
            arch: CommonArch,
            msvm_fd: WriteVar<PathBuf>
        }
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let version = config.version;
        let local_paths = config.local_paths;
        let mut reqs: BTreeMap<CommonArch, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::GetMsvmFd { arch, msvm_fd } => reqs.entry(arch).or_default().push(msvm_fd),
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local mu_msvm UEFI", |ctx| {
                let reqs = reqs.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(arch, var)| (arch, var.claim(ctx)))
                    .collect();
                move |rt| {
                    for (arch, out_vars) in reqs {
                        let msvm_fd_var = local_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for architecture {:?}", arch)
                        })?;
                        let msvm_fd = rt.read(msvm_fd_var.clone());
                        for var in out_vars {
                            log::info!(
                                "using local uefi for {} at path {:?}",
                                match arch {
                                    CommonArch::X86_64 => "x64",
                                    CommonArch::Aarch64 => "aarch64",
                                },
                                msvm_fd
                            );
                            rt.write(var, &msvm_fd);
                        }
                    }
                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local paths handled above");
        let extract_archive_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for (arch, out_vars) in reqs {
            let file_name = match arch {
                CommonArch::X86_64 => "RELEASE-X64-VS2022-artifacts.tar.gz",
                CommonArch::Aarch64 => "RELEASE-AARCH64-CLANGPDB-artifacts.tar.gz",
            };

            let mu_msvm_archive = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "mu_msvm".into(),
                needs_auth: false,
                tag: format!("v{version}"),
                file_name: file_name.into(),
                path: v,
            });

            let archive_file_version = format!("{version}-{file_name}");

            ctx.emit_rust_step(
                {
                    format!(
                        "unpack mu_msvm package ({})",
                        match arch {
                            CommonArch::X86_64 => "x64",
                            CommonArch::Aarch64 => "aarch64",
                        },
                    )
                },
                |ctx| {
                    let extract_archive_deps = extract_archive_deps.clone().claim(ctx);
                    let out_vars = out_vars.claim(ctx);
                    let mu_msvm_archive = mu_msvm_archive.claim(ctx);
                    move |rt| {
                        let mu_msvm_archive = rt.read(mu_msvm_archive);

                        let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                            rt,
                            extract_archive_deps,
                            &mu_msvm_archive,
                            &archive_file_version,
                        )?;

                        let msvm_fd = extract_dir.join("FV/MSVM.fd");

                        for var in out_vars {
                            rt.write(var, &msvm_fd)
                        }

                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}

/// Resolve and share a single latest Patina release across a pipeline's jobs.
pub mod latest_patina {
    use crate::common::CommonArch;
    use flowey::node::prelude::*;

    flowey_request! {
        pub enum Request {
            Download {
                artifact_dir: ReadVar<PathBuf>,
                done: WriteVar<SideEffect>,
            },
            UseFirmware {
                artifact_dir: ReadVar<PathBuf>,
                arch: CommonArch,
            },
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::use_gh_cli::Node>();
            ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
            ctx.import::<crate::_jobs::cfg_versions::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            match request {
                Request::UseFirmware { artifact_dir, arch } => {
                    let path = artifact_dir.map(ctx, move |dir| dir.join(firmware_name(arch)));
                    ctx.req(crate::_jobs::cfg_versions::Request::LocalUefi(arch, path));
                }
                Request::Download { artifact_dir, done } => {
                    let gh_cli = ctx.reqv(flowey_lib_common::use_gh_cli::Request::Get);
                    let extract_deps =
                        flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);
                    ctx.emit_rust_step("download latest Patina ClangPDB firmware", |ctx| {
                        let gh_cli = gh_cli.claim(ctx);
                        let artifact_dir = artifact_dir.claim(ctx);
                        let extract_deps = extract_deps.claim(ctx);
                        done.claim(ctx);
                        move |rt| {
                            let gh_cli = rt.read(gh_cli);
                            let artifact_dir = rt.read(artifact_dir);
                            let release_json = flowey::shell_cmd!(
                                rt,
                                "{gh_cli} api repos/microsoft/mu_msvm/releases/latest"
                            )
                            .read()?;
                            #[derive(Deserialize)]
                            struct Release {
                                tag_name: String,
                            }
                            let release: Release = serde_json::from_str(&release_json)?;
                            let tag = release.tag_name;
                            log::info!("using mu_msvm Patina release {tag}");
                            fs_err::create_dir_all(&artifact_dir)?;
                            fs_err::write(artifact_dir.join("release.json"), release_json)?;

                            let working_dir = rt.sh.current_dir();
                            for (arch, arch_tag) in [
                                (CommonArch::X86_64, "X64"),
                                (CommonArch::Aarch64, "AARCH64"),
                            ] {
                                rt.sh.change_dir(&working_dir);
                                let file_name =
                                    format!("firmware-RELEASE-{arch_tag}-CLANGPDB-patina.tar.gz");
                                flowey::shell_cmd!(
                                    rt,
                                    "{gh_cli} release download --repo microsoft/mu_msvm {tag} --pattern {file_name} --clobber"
                                )
                                .run()?;
                                let archive = rt.sh.current_dir().join(&file_name);
                                let extract_dir =
                                    flowey_lib_common::_util::extract::extract_zip_if_new(
                                        rt,
                                        extract_deps.clone(),
                                        &archive,
                                        &format!("{tag}-{file_name}"),
                                    )?;
                                fs_err::copy(
                                    extract_dir.join("FV/MSVM.fd"),
                                    artifact_dir.join(firmware_name(arch)),
                                )?;
                            }
                            Ok(())
                        }
                    });
                }
            }
            Ok(())
        }
    }

    fn firmware_name(arch: CommonArch) -> &'static str {
        match arch {
            CommonArch::X86_64 => "MSVM-X64.fd",
            CommonArch::Aarch64 => "MSVM-AARCH64.fd",
        }
    }
}
