// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download various pre-built `openvmm-deps` dependencies, or use a local path if specified.
//!
//! The openvmm-deps release publishes separate archives:
//! - `openvmm-deps.{arch}.{ver}.tar.gz` — SDK tools (dbgrd, shell, sysroot, petritools)
//! - `openvmm-test-initrd.{arch}.{ver}.tar.gz` — shared test initrd
//! - `openvmm-test-linux-{kernel_ver}.{arch}.{ver}.tar.gz` — test kernel

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Which file to extract from the openvmm-deps archive.
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenvmmDepFile {
    OpenhclCpioDbgrd,
    OpenhclCpioShell,
    OpenhclSysroot,
    PetritoolsErofs,
}

impl OpenvmmDepFile {
    pub fn filename(self) -> &'static str {
        match self {
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
                        let path = base_dir.join(dep.filename());
                        rt.write_all(vars, &path)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local requests handled above");

        // Determine which architectures we need to download.
        let needed_archs: BTreeSet<CommonArch> = deps.keys().map(|(_, arch)| *arch).collect();

        let persistent_dir = ctx.persistent_dir();

        // Download each unique architecture.
        let downloads: BTreeMap<CommonArch, ReadVar<PathBuf>> = needed_archs
            .into_iter()
            .map(|arch| {
                let arch_str = match arch {
                    CommonArch::X86_64 => "x86_64",
                    CommonArch::Aarch64 => "aarch64",
                };
                let file_name = format!("openvmm-deps.{arch_str}.{version}.tar.gz");
                let path = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "openvmm-deps".into(),
                    needs_auth: false,
                    tag: version.clone(),
                    file_name,
                    path: v,
                });
                (arch, path)
            })
            .collect();

        ctx.emit_rust_step("unpack openvmm-deps archive", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let downloads: BTreeMap<_, _> = downloads
                .into_iter()
                .map(|(key, var)| (key, var.claim(ctx)))
                .collect();
            let deps = deps.claim(ctx);
            let version = version.clone();
            move |rt| {
                let persistent_dir = persistent_dir.map(|d| rt.read(d));

                // Extract each downloaded archive, keyed by architecture.
                let extract_dirs: BTreeMap<CommonArch, PathBuf> = downloads
                    .into_iter()
                    .map(|(arch, var)| {
                        let file = rt.read(var);
                        let dir = flowey_lib_common::_util::extract::extract_tar_gz_if_new(
                            rt,
                            persistent_dir.as_deref(),
                            &file,
                            &version,
                        )?;
                        Ok((arch, dir))
                    })
                    .collect::<anyhow::Result<_>>()?;

                for ((dep, arch), vars) in deps {
                    let extract_dir = extract_dirs
                        .get(&arch)
                        .expect("archive was downloaded for this arch");
                    let path = extract_dir.join(dep.filename());
                    rt.write_all(vars, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
