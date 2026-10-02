// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve OpenHCL kernel packages - either by downloading from GitHub Release
//! or using local paths

use crate::common::CommonArch;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy)]
pub enum OpenhclKernelPackageKind {
    Main,
    Cvm,
    Dev,
    CvmDev,
}

impl OpenhclKernelPackageKind {
    pub fn is_dev(self) -> bool {
        match self {
            Self::Main | Self::Cvm => false,
            Self::Dev | Self::CvmDev => true,
        }
    }
}

flowey_config! {
    /// Config for the resolve_openhcl_kernel_package node.
    pub struct Config {
        /// Version strings keyed by package kind.
        pub versions: BTreeMap<OpenhclKernelPackageKind, String>,
        /// Local paths keyed by architecture (kernel binary, modules directory).
        pub local_paths: BTreeMap<CommonArch, (ConfigVar<PathBuf>, ConfigVar<PathBuf>)>,
    }
}

flowey_request! {
    #[expect(clippy::enum_variant_names)]
    pub enum Request {
        /// Get path to the kernel binary
        GetKernel {
            kind: OpenhclKernelPackageKind,
            arch: CommonArch,
            kernel: WriteVar<PathBuf>,
        },
        /// Get path to the kernel modules directory
        GetModules {
            kind: OpenhclKernelPackageKind,
            arch: CommonArch,
            modules: WriteVar<PathBuf>,
        },
        /// Get path to the package root (for metadata files, etc)
        GetPackageRoot {
            kind: OpenhclKernelPackageKind,
            arch: CommonArch,
            pkg: WriteVar<PathBuf>,
        },
        /// Get path to the kernel build metadata file
        GetMetadata {
            kind: OpenhclKernelPackageKind,
            arch: CommonArch,
            metadata: WriteVar<PathBuf>,
        },
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
        let versions = config.versions;
        let local_paths = config.local_paths;
        let mut kernel_reqs: BTreeMap<
            (OpenhclKernelPackageKind, CommonArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();
        let mut modules_reqs: BTreeMap<
            (OpenhclKernelPackageKind, CommonArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();
        let mut pkg_reqs: BTreeMap<(OpenhclKernelPackageKind, CommonArch), Vec<WriteVar<PathBuf>>> =
            BTreeMap::new();
        let mut metadata_reqs: BTreeMap<
            (OpenhclKernelPackageKind, CommonArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::GetKernel { kind, arch, kernel } => {
                    kernel_reqs.entry((kind, arch)).or_default().push(kernel);
                }
                Request::GetModules {
                    kind,
                    arch,
                    modules,
                } => {
                    modules_reqs.entry((kind, arch)).or_default().push(modules);
                }
                Request::GetPackageRoot { kind, arch, pkg } => {
                    pkg_reqs.entry((kind, arch)).or_default().push(pkg);
                }
                Request::GetMetadata {
                    kind,
                    arch,
                    metadata,
                } => {
                    metadata_reqs
                        .entry((kind, arch))
                        .or_default()
                        .push(metadata);
                }
            }
        }

        // Collect all architectures that need resolution
        let all_reqs: std::collections::BTreeSet<(OpenhclKernelPackageKind, CommonArch)> =
            kernel_reqs
                .keys()
                .chain(modules_reqs.keys())
                .chain(pkg_reqs.keys())
                .chain(metadata_reqs.keys())
                .copied()
                .collect();

        // Verify we have either local paths or versions for each requested architecture
        for (kind, arch) in &all_reqs {
            if !local_paths.contains_key(arch) && !versions.contains_key(kind) {
                if kind.is_dev() {
                    anyhow::bail!(
                        "OpenHCL dev kernel support is disabled; provide local kernel paths for \
                         {:?} to enable {:?}",
                        arch,
                        kind,
                    );
                }
                anyhow::bail!(
                    "Must provide either SetLocal for {:?} or SetVersion for {:?}",
                    arch,
                    kind
                );
            }
        }

        if all_reqs.is_empty() {
            return Ok(());
        }

        // Partition requests into local vs download
        let (local_reqs, download_reqs): (Vec<_>, Vec<_>) = all_reqs
            .into_iter()
            .partition(|(_, arch)| local_paths.contains_key(arch));

        // Split the request maps into local and download portions
        let (kernel_reqs_local, mut kernel_reqs_download): (BTreeMap<_, _>, BTreeMap<_, _>) =
            kernel_reqs
                .into_iter()
                .partition(|((_, arch), _)| local_paths.contains_key(arch));
        let (modules_reqs_local, mut modules_reqs_download): (BTreeMap<_, _>, BTreeMap<_, _>) =
            modules_reqs
                .into_iter()
                .partition(|((_, arch), _)| local_paths.contains_key(arch));
        let (pkg_reqs_local, mut pkg_reqs_download): (BTreeMap<_, _>, BTreeMap<_, _>) = pkg_reqs
            .into_iter()
            .partition(|((_, arch), _)| local_paths.contains_key(arch));
        let (metadata_reqs_local, mut metadata_reqs_download): (BTreeMap<_, _>, BTreeMap<_, _>) =
            metadata_reqs
                .into_iter()
                .partition(|((_, arch), _)| local_paths.contains_key(arch));

        // Handle local paths
        if !local_reqs.is_empty() {
            ctx.emit_rust_step("use local kernel package", |ctx| {
                let mut kernel_reqs = kernel_reqs_local.claim(ctx);
                let mut modules_reqs = modules_reqs_local.claim(ctx);
                let mut pkg_reqs = pkg_reqs_local.claim(ctx);
                let mut metadata_reqs = metadata_reqs_local.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(arch, (k, m))| (arch, (k.claim(ctx), m.claim(ctx))))
                    .collect();
                let local_reqs = group_by(local_reqs.into_iter().map(|(kind, arch)| (arch, kind)));

                move |rt| {
                    for (arch, kinds) in local_reqs {
                        let (kernel_var, modules_var) = local_paths.get(&arch).unwrap();
                        let kernel_path = rt.read(kernel_var.clone());
                        let modules_path = rt.read(modules_var.clone());

                        log::info!(
                            "using local kernel at {:?} and modules at {:?}",
                            kernel_path,
                            modules_path
                        );

                        let package_root = kernel_path.parent().map(Path::to_path_buf);
                        let metadata_path = package_root
                            .as_ref()
                            .map(|root| root.join("kernel_build_metadata.json"));
                        for kind in kinds {
                            if let Some(vars) = kernel_reqs.remove(&(kind, arch)) {
                                rt.write_all(vars, &kernel_path);
                            }
                            if let Some(vars) = modules_reqs.remove(&(kind, arch)) {
                                rt.write_all(vars, &modules_path);
                            }
                            if let Some(vars) = pkg_reqs.remove(&(kind, arch)) {
                                rt.write_all(
                                    vars,
                                    package_root
                                        .as_ref()
                                        .context("local kernel path has no package directory")?,
                                );
                            }
                            if let Some(vars) = metadata_reqs.remove(&(kind, arch)) {
                                rt.write_all(
                                    vars,
                                    metadata_path
                                        .as_ref()
                                        .context("local kernel path has no metadata directory")?,
                                );
                            }
                        }
                    }
                    Ok(())
                }
            });
        }

        if download_reqs.is_empty() {
            return Ok(());
        }

        // Handle downloads
        let extract_zip_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for (kind, arch) in download_reqs {
            let version = versions.get(&kind).expect("checked above");
            let tag = format!(
                "rolling-lts/hcl-{}/{}",
                match kind {
                    OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Cvm => "main",
                    OpenhclKernelPackageKind::Dev | OpenhclKernelPackageKind::CvmDev => "dev",
                },
                version
            );

            let file_name = format!(
                "Microsoft.OHCL.Kernel{}.{}{}-{}.tar.gz",
                match kind {
                    OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Cvm => "",
                    OpenhclKernelPackageKind::Dev | OpenhclKernelPackageKind::CvmDev => ".Dev",
                },
                version,
                match kind {
                    OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Dev => "",
                    OpenhclKernelPackageKind::Cvm | OpenhclKernelPackageKind::CvmDev => "-cvm",
                },
                match arch {
                    CommonArch::X86_64 => "x64",
                    CommonArch::Aarch64 => "arm64",
                },
            );

            let kernel_package_tar_gz =
                ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "OHCL-Linux-Kernel".into(),
                    needs_auth: false,
                    tag,
                    file_name: file_name.clone(),
                    path: v,
                });

            let kernel_file_name = match arch {
                CommonArch::X86_64 => "vmlinux",
                CommonArch::Aarch64 => "Image",
            };

            ctx.emit_rust_step("extract and resolve kernel package", |ctx| {
                let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
                let kernel_vars = kernel_reqs_download.remove(&(kind, arch)).claim(ctx);
                let modules_vars = modules_reqs_download.remove(&(kind, arch)).claim(ctx);
                let pkg_vars = pkg_reqs_download.remove(&(kind, arch)).claim(ctx);
                let metadata_vars = metadata_reqs_download.remove(&(kind, arch)).claim(ctx);
                let kernel_package_tar_gz = kernel_package_tar_gz.claim(ctx);

                move |rt| {
                    let kernel_package_tar_gz = rt.read(kernel_package_tar_gz);

                    // Extract the downloaded package
                    let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                        rt,
                        extract_zip_deps,
                        &kernel_package_tar_gz,
                        &file_name,
                    )?;

                    // The extracted directory contains: vmlinux/Image, modules/, kernel_build_metadata.json
                    let kernel_path = extract_dir.join(kernel_file_name);
                    let modules_path = extract_dir.join("modules");
                    let metadata_path = extract_dir.join("kernel_build_metadata.json");

                    if let Some(vars) = kernel_vars {
                        rt.write_all(vars, &kernel_path);
                    }
                    if let Some(vars) = modules_vars {
                        rt.write_all(vars, &modules_path);
                    }
                    if let Some(vars) = pkg_vars {
                        rt.write_all(vars, &extract_dir);
                    }
                    if let Some(vars) = metadata_vars {
                        rt.write_all(vars, &metadata_path);
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
