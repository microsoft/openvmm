// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve OpenHCL kernel packages - either by downloading from GitHub Release
//! or using local paths

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy)]
pub enum OpenhclKernelPackageKind {
    Main,
    Cvm,
    Dev,
    CvmDev,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy)]
pub enum OpenhclKernelPackageArch {
    X86_64,
    Aarch64,
}

flowey_request! {
    pub enum Request {
        /// Set local paths for a specific architecture
        SetLocal {
            arch: OpenhclKernelPackageArch,
            kernel: PathBuf,
            modules: PathBuf,
        },
        /// Specify version string to use for each package kind
        SetVersion(OpenhclKernelPackageKind, String),
        /// Get path to the kernel binary
        GetKernel {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            kernel: WriteVar<PathBuf>,
        },
        /// Get path to the kernel modules directory
        GetModules {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            modules: WriteVar<PathBuf>,
        },
        /// Get path to the package root (for metadata files, etc)
        GetPackageRoot {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            pkg: WriteVar<PathBuf>,
        },
        /// Get path to the kernel build metadata file
        GetMetadata {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            metadata: WriteVar<PathBuf>,
        },
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut versions: BTreeMap<OpenhclKernelPackageKind, String> = BTreeMap::new();
        let mut local_paths: BTreeMap<OpenhclKernelPackageArch, (PathBuf, PathBuf)> =
            BTreeMap::new();
        let mut kernel_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();
        let mut modules_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();
        let mut pkg_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();
        let mut metadata_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::SetVersion(kind, v) => {
                    let mut old = versions.insert(kind, v.clone());
                    same_across_all_reqs("SetVersion", &mut old, v)?
                }
                Request::SetLocal {
                    arch,
                    kernel,
                    modules,
                } => {
                    if let Some(existing) = local_paths.get(&arch) {
                        if existing != &(kernel.clone(), modules.clone()) {
                            anyhow::bail!("Conflicting local paths for {:?}", arch);
                        }
                    } else {
                        local_paths.insert(arch, (kernel, modules));
                    }
                }
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
        let all_reqs: std::collections::BTreeSet<(
            OpenhclKernelPackageKind,
            OpenhclKernelPackageArch,
        )> = kernel_reqs
            .keys()
            .chain(modules_reqs.keys())
            .chain(pkg_reqs.keys())
            .chain(metadata_reqs.keys())
            .cloned()
            .collect();

        // Verify we have either local paths or versions for each requested architecture
        for (kind, arch) in &all_reqs {
            if !local_paths.contains_key(arch) && !versions.contains_key(kind) {
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
                let local_paths = local_paths.clone();
                let local_reqs = local_reqs.clone();

                move |rt| {
                    for (_, arch) in local_reqs {
                        let (kernel_path, modules_path) = local_paths.get(&arch).unwrap();

                        log::info!(
                            "using local kernel at {:?} and modules at {:?}",
                            kernel_path,
                            modules_path
                        );

                        // Write kernel paths for all kinds matching this arch
                        for kind in [
                            OpenhclKernelPackageKind::Main,
                            OpenhclKernelPackageKind::Dev,
                            OpenhclKernelPackageKind::Cvm,
                            OpenhclKernelPackageKind::CvmDev,
                        ] {
                            if let Some(vars) = kernel_reqs.remove(&(kind, arch)) {
                                rt.write_all(vars, kernel_path);
                            }
                        }

                        // Write modules paths for all kinds matching this arch
                        for kind in [
                            OpenhclKernelPackageKind::Main,
                            OpenhclKernelPackageKind::Dev,
                            OpenhclKernelPackageKind::Cvm,
                            OpenhclKernelPackageKind::CvmDev,
                        ] {
                            if let Some(vars) = modules_reqs.remove(&(kind, arch)) {
                                rt.write_all(vars, modules_path);
                            }
                        }

                        // Write package root paths (parent of kernel)
                        if let Some(parent) = kernel_path.parent() {
                            let parent_buf = parent.to_path_buf();
                            for kind in [
                                OpenhclKernelPackageKind::Main,
                                OpenhclKernelPackageKind::Dev,
                                OpenhclKernelPackageKind::Cvm,
                                OpenhclKernelPackageKind::CvmDev,
                            ] {
                                if let Some(vars) = pkg_reqs.remove(&(kind, arch)) {
                                    rt.write_all(vars, &parent_buf);
                                }
                            }

                            // Write metadata paths (kernel_build_metadata.json in same dir as kernel)
                            let metadata_path = parent_buf.join("kernel_build_metadata.json");
                            for kind in [
                                OpenhclKernelPackageKind::Main,
                                OpenhclKernelPackageKind::Dev,
                                OpenhclKernelPackageKind::Cvm,
                                OpenhclKernelPackageKind::CvmDev,
                            ] {
                                if let Some(vars) = metadata_reqs.remove(&(kind, arch)) {
                                    rt.write_all(vars, &metadata_path);
                                }
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
                    OpenhclKernelPackageArch::X86_64 => "x64",
                    OpenhclKernelPackageArch::Aarch64 => "arm64",
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
                OpenhclKernelPackageArch::X86_64 => "vmlinux",
                OpenhclKernelPackageArch::Aarch64 => "Image",
            };

            let has_kernel_req = kernel_reqs_download.contains_key(&(kind, arch));
            let has_modules_req = modules_reqs_download.contains_key(&(kind, arch));
            let has_pkg_req = pkg_reqs_download.contains_key(&(kind, arch));
            let has_metadata_req = metadata_reqs_download.contains_key(&(kind, arch));

            ctx.emit_rust_step("extract and resolve kernel package", |ctx| {
                let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
                let kernel_vars = if has_kernel_req {
                    Some(
                        kernel_reqs_download
                            .remove(&(kind, arch))
                            .unwrap()
                            .claim(ctx),
                    )
                } else {
                    None
                };
                let modules_vars = if has_modules_req {
                    Some(
                        modules_reqs_download
                            .remove(&(kind, arch))
                            .unwrap()
                            .claim(ctx),
                    )
                } else {
                    None
                };
                let pkg_vars = if has_pkg_req {
                    Some(pkg_reqs_download.remove(&(kind, arch)).unwrap().claim(ctx))
                } else {
                    None
                };
                let metadata_vars = if has_metadata_req {
                    Some(
                        metadata_reqs_download
                            .remove(&(kind, arch))
                            .unwrap()
                            .claim(ctx),
                    )
                } else {
                    None
                };
                let kernel_package_tar_gz = kernel_package_tar_gz.claim(ctx);
                let file_name = file_name.clone();
                let kernel_file_name = kernel_file_name.to_string();

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
                    let kernel_path = extract_dir.join(&kernel_file_name);
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
