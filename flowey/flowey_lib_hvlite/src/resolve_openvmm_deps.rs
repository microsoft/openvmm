// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download various pre-built `openvmm-deps` dependencies, or use a local path if specified.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

/// Which file to extract from the openvmm-deps archive.
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenvmmDepFile {
    LinuxTestKernel,
    LinuxTestInitrd,
    OpenhclCpioDbgrd,
    OpenhclCpioShell,
    OpenhclSysroot,
    PetritoolsErofs,
}

impl OpenvmmDepFile {
    pub fn filename(self, arch: CommonArch) -> &'static str {
        match self {
            Self::LinuxTestKernel => match arch {
                CommonArch::X86_64 => "vmlinux",
                CommonArch::Aarch64 => "Image",
            },
            Self::LinuxTestInitrd => "initrd",
            Self::OpenhclCpioDbgrd => "dbgrd.cpio.gz",
            Self::OpenhclCpioShell => "shell.cpio.gz",
            Self::OpenhclSysroot => "sysroot.tar.gz",
            Self::PetritoolsErofs => "petritools.erofs",
        }
    }
}

flowey_config! {
    /// Config for the resolve_openvmm_deps node.
    pub struct Config {
        /// Specify version of the github release to pull from
        pub version: Option<String>,
        /// Use locally downloaded openvmm-deps, keyed by architecture
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to a specific dep file
        Get(OpenvmmDepFile, CommonArch, WriteVar<PathBuf>),
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
        let mut deps: BTreeMap<(OpenvmmDepFile, CommonArch), Vec<WriteVar<PathBuf>>> =
            BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(dep, arch, var) => {
                    deps.entry((dep, arch)).or_default().push(var);
                }
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if deps.is_empty() {
            return Ok(());
        }

        // Which architectures have at least one dep requested?
        let needs_arch = |arch: CommonArch| deps.keys().any(|(_, a)| *a == arch);

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local openvmm-deps", |ctx| {
                let deps = deps.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(arch, var)| (arch, var.claim(ctx)))
                    .collect();
                move |rt| {
                    let resolved_paths: BTreeMap<CommonArch, PathBuf> = local_paths
                        .into_iter()
                        .map(|(arch, var)| (arch, rt.read(var)))
                        .collect();

                    for ((dep, arch), vars) in deps {
                        let base_dir = resolved_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for architecture {:?}", arch)
                        })?;
                        let path = base_dir.join(dep.filename(arch));
                        rt.write_all(vars, &path)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        let extract_tar_bz2_deps =
            flowey_lib_common::_util::extract::extract_tar_bz2_if_new_deps(ctx);

        let download_archive = |arch: CommonArch, ctx: &mut NodeCtx<'_>| {
            let version = version.clone().expect("local requests handled above");
            let arch_str = match arch {
                CommonArch::X86_64 => "x86_64",
                CommonArch::Aarch64 => "aarch64",
            };
            ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: version.clone(),
                file_name: format!("openvmm-deps.{arch_str}.{version}.tar.gz"),
                path: v,
            })
        };

        let openvmm_deps_tar_bz2_x64 =
            needs_arch(CommonArch::X86_64).then(|| download_archive(CommonArch::X86_64, ctx));
        let openvmm_deps_tar_bz2_aarch64 =
            needs_arch(CommonArch::Aarch64).then(|| download_archive(CommonArch::Aarch64, ctx));

        ctx.emit_rust_step("unpack openvmm-deps archive", |ctx| {
            let extract_tar_bz2_deps = extract_tar_bz2_deps.claim(ctx);
            let openvmm_deps_tar_bz2_x64 = openvmm_deps_tar_bz2_x64.claim(ctx);
            let openvmm_deps_tar_bz2_aarch64 = openvmm_deps_tar_bz2_aarch64.claim(ctx);
            let deps = deps.claim(ctx);
            let version = version.clone().expect("local requests handled above");
            move |rt| {
                let extract_dir_x64 = openvmm_deps_tar_bz2_x64
                    .map(|file| {
                        let file = rt.read(file);
                        flowey_lib_common::_util::extract::extract_tar_bz2_if_new(
                            rt,
                            extract_tar_bz2_deps.clone(),
                            &file,
                            &version,
                        )
                    })
                    .transpose()?;
                let extract_dir_aarch64 = openvmm_deps_tar_bz2_aarch64
                    .map(|file| {
                        let file = rt.read(file);
                        flowey_lib_common::_util::extract::extract_tar_bz2_if_new(
                            rt,
                            extract_tar_bz2_deps.clone(),
                            &file,
                            &version,
                        )
                    })
                    .transpose()?;

                let base_dir = |arch| match arch {
                    CommonArch::X86_64 => extract_dir_x64.clone().unwrap(),
                    CommonArch::Aarch64 => extract_dir_aarch64.clone().unwrap(),
                };

                for ((dep, arch), vars) in deps {
                    let path = base_dir(arch).join(dep.filename(arch));
                    rt.write_all(vars, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
