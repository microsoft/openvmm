// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the shared test initrd from the `openvmm-deps` GitHub release,
//! or use a local path if specified.
//!
//! The initrd is identical across all kernel versions and ships as its own
//! versioned artifact (`openvmm-test-initrd.<arch>.<ver>.tar.gz`), separate
//! from the per-kernel `openvmm-test-linux-<kver>` artifact.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

flowey_config! {
    /// Config for the resolve_openvmm_test_initrd node.
    pub struct Config {
        /// Specify version of the github release to pull from
        pub version: Option<String>,
        /// Use locally downloaded openvmm-test-initrd contents, keyed by
        /// architecture
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to the initrd image for a given architecture
        Get(CommonArch, WriteVar<PathBuf>),
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
        let mut deps: BTreeMap<CommonArch, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(arch, var) => {
                    deps.entry(arch).or_default().push(var);
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
            ctx.emit_rust_step("use local openvmm-test-initrd", |ctx| {
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

                    for (arch, vars) in deps {
                        let base_dir = resolved_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for {:?}", arch)
                        })?;
                        let path = base_dir.join("initrd");
                        rt.write_all(vars, &path)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        // The openvmm-test-initrd.<arch>.<ver>.tar.gz archive contains a
        // single `initrd` file at the archive root. Download one archive per
        // requested architecture.
        let needed_archives: BTreeSet<CommonArch> = deps.keys().copied().collect();

        let mut archives = BTreeMap::new();
        for arch in needed_archives {
            let version = version.clone().expect("local requests handled above");
            let arch_str = match arch {
                CommonArch::X86_64 => "x86_64",
                CommonArch::Aarch64 => "aarch64",
            };
            let archive = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: version.clone(),
                file_name: format!("openvmm-test-initrd.{arch_str}.{version}.tar.gz"),
                path: v,
            });
            archives.insert(arch, archive);
        }

        let persistent_dir = ctx.persistent_dir();

        ctx.emit_rust_step("unpack openvmm-test-initrd archives", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let archives = archives.claim(ctx);
            let deps = deps.claim(ctx);
            let version = version.clone().expect("local requests handled above");
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

                for (arch, vars) in deps {
                    let path = extract_dirs[&arch].join("initrd");
                    rt.write_all(vars, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
