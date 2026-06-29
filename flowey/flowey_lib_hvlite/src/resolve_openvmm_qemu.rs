// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the statically-linked QEMU binary from the `openvmm-deps`
//! GitHub release, or use a local path if specified.
//!
//! Each release publishes `qemu-linux-static.<host_arch>.<ver>.tar.gz`
//! archives containing QEMU system-emulation binaries (e.g.,
//! `qemu-system-aarch64`) built for the given host architecture.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Which QEMU binary to extract from the archive.
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum QemuFile {
    /// `qemu-system-aarch64` — emulates aarch64 guests.
    SystemAarch64,
}

impl QemuFile {
    /// The filename of this binary inside the archive.
    pub fn filename(self) -> &'static str {
        match self {
            Self::SystemAarch64 => "qemu-system-aarch64",
        }
    }
}

flowey_config! {
    /// Config for the resolve_openvmm_qemu node.
    pub struct Config {
        /// Specify version of the github release to pull from
        pub version: Option<String>,
        /// Use locally downloaded QEMU binaries, keyed by host architecture
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to a QEMU binary for the given host architecture.
        Get(QemuFile, CommonArch, WriteVar<PathBuf>),
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
        let Config {
            version,
            local_paths,
        } = config;
        let mut deps: BTreeMap<(QemuFile, CommonArch), Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(file, arch, var) => {
                    deps.entry((file, arch)).or_default().push(var);
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
            ctx.emit_rust_step("use local QEMU", |ctx| {
                let deps = deps.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(key, var)| (key, var.claim(ctx)))
                    .collect();
                move |rt| {
                    let resolved_paths: BTreeMap<CommonArch, PathBuf> = local_paths
                        .into_iter()
                        .map(|(key, var)| (key, rt.read(var)))
                        .collect();

                    for ((file, arch), vars) in deps {
                        let base_dir = resolved_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for {:?}", arch)
                        })?;
                        let path = base_dir.join(file.filename());
                        rt.write_all(vars, &path)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local requests handled above");

        // Deduplicate downloads per host architecture.
        let needed_archives: BTreeSet<CommonArch> = deps.keys().map(|(_, arch)| *arch).collect();

        let mut archives = BTreeMap::new();
        for arch in needed_archives {
            let arch_str = match arch {
                CommonArch::X86_64 => "x86_64",
                CommonArch::Aarch64 => "aarch64",
            };
            let archive = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: version.clone(),
                file_name: format!("qemu-linux-static.{arch_str}.{version}.tar.gz"),
                path: v,
            });
            archives.insert(arch, archive);
        }

        let persistent_dir = ctx.persistent_dir();

        ctx.emit_rust_step("unpack QEMU archive", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let archives = archives.claim(ctx);
            let deps = deps.claim(ctx);
            let version = version.clone();
            move |rt| {
                let persistent_dir = persistent_dir.map(|d| rt.read(d));

                let mut extract_dirs = BTreeMap::new();
                for (arch, archive) in archives {
                    let file = rt.read(archive);
                    let dir = flowey_lib_common::_util::extract::extract_tar_gz_if_new(
                        rt,
                        persistent_dir.as_deref(),
                        &file,
                        &version,
                    )?;
                    extract_dirs.insert(arch, dir);
                }

                for ((file, arch), vars) in deps {
                    let extract_dir = extract_dirs
                        .get(&arch)
                        .expect("archive was downloaded for this arch");
                    let path = extract_dir.join(file.filename());
                    if !path.exists() {
                        anyhow::bail!(
                            "expected QEMU binary not found in archive: {}",
                            path.display()
                        );
                    }
                    // Ensure the binary is executable
                    path.make_executable()?;
                    rt.write_all(vars, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
