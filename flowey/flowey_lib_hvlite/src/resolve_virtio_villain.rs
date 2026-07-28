// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the virtio-villain guest test artifact from the `openvmm-deps`
//! GitHub release, or use a local directory if specified.
//!
//! virtio-villain ships as its own versioned artifact
//! (`openvmm-test-virtio-villain.<arch>.<ver>.tar.gz`) containing the guest
//! `initramfs.cpio.gz` (a static musl `init` that drives the virtio fault
//! injection) and `tests.tsv` (the enumerated test list). The
//! `virtio_villain_tests` crate consumes these two files via the
//! `VILLAIN_INITRAMFS` / `VILLAIN_TSV` env vars.
//!
//! By default this resolves the artifact from the `openvmm-deps` release pinned
//! by [`crate::_jobs::cfg_versions::OPENVMM_DEPS`]; pass a local path override to
//! use a locally built artifact instead.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Resolved paths to the two files that make up the virtio-villain artifact.
#[derive(Clone, Serialize, Deserialize)]
pub struct VirtioVillainArtifact {
    /// Path to `initramfs.cpio.gz`.
    pub initramfs: PathBuf,
    /// Path to `tests.tsv`.
    pub tsv: PathBuf,
}

flowey_config! {
    /// Config for the resolve_virtio_villain node.
    pub struct Config {
        /// Specify version of the github release to pull from.
        pub version: Option<String>,
        /// Use a locally-downloaded virtio-villain artifact directory (which
        /// must contain `initramfs.cpio.gz` and `tests.tsv`), keyed by
        /// architecture.
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the resolved virtio-villain artifact for a given architecture.
        Get(CommonArch, WriteVar<VirtioVillainArtifact>),
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
        let mut deps: BTreeMap<CommonArch, Vec<WriteVar<VirtioVillainArtifact>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(arch, var) => {
                    deps.entry(arch).or_default().push(var);
                }
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("cannot specify both `version` and `local_paths`");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("must specify either `version` or `local_paths`");
        }

        // -- end of req processing -- //

        if deps.is_empty() {
            return Ok(());
        }

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local virtio-villain artifact", |ctx| {
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
                        let artifact = resolve_from_dir(base_dir)?;
                        rt.write_all(vars, &artifact)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        // The openvmm-test-virtio-villain.<arch>.<ver>.tar.gz archive contains
        // `initramfs.cpio.gz` and `tests.tsv` at the archive root. Download one
        // archive per requested architecture.
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
                file_name: format!("openvmm-test-virtio-villain.{arch_str}.{version}.tar.gz"),
                path: v,
            });
            archives.insert(arch, archive);
        }

        let persistent_dir = ctx.persistent_dir();

        ctx.emit_rust_step("unpack virtio-villain artifacts", |ctx| {
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
                    let artifact = resolve_from_dir(&extract_dirs[&arch])?;
                    rt.write_all(vars, &artifact)
                }

                Ok(())
            }
        });

        Ok(())
    }
}

/// Resolve the two virtio-villain files from a directory, erroring if either is
/// missing.
fn resolve_from_dir(dir: &Path) -> anyhow::Result<VirtioVillainArtifact> {
    let initramfs = dir.join("initramfs.cpio.gz");
    let tsv = dir.join("tests.tsv");
    if !initramfs.exists() {
        anyhow::bail!(
            "virtio-villain initramfs.cpio.gz not found in {}",
            dir.display()
        );
    }
    if !tsv.exists() {
        anyhow::bail!("virtio-villain tests.tsv not found in {}", dir.display());
    }
    Ok(VirtioVillainArtifact { initramfs, tsv })
}
