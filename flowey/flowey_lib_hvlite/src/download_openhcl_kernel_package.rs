// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built OpenHCL kernel packages from their GitHub Release

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenhclKernelPackageKind {
    Main,
    Cvm,
    Dev,
    CvmDev,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenhclKernelPackageArch {
    X86_64,
    Aarch64,
}

flowey_request! {
    pub enum Request {
        /// Use a locally provided kernel package directory
        LocalPath(ReadVar<PathBuf>),
        /// Specify version string to use for each package kind
        Version(OpenhclKernelPackageKind, String),
        /// Download the specified kernel package
        GetPackage {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            pkg: WriteVar<PathBuf>
        }
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
        let mut local_path: Option<ReadVar<PathBuf>> = None;
        let mut versions: BTreeMap<OpenhclKernelPackageKind, String> = BTreeMap::new();
        let mut reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::LocalPath(path) => {
                    same_across_all_reqs_backing_var("LocalPath", &mut local_path, path)?
                }
                Request::Version(arch, v) => {
                    let mut old = versions.insert(arch, v.clone());
                    same_across_all_reqs("SetVersion", &mut old, v)?
                }
                Request::GetPackage { kind, arch, pkg } => {
                    reqs.entry((kind, arch)).or_default().push(pkg)
                }
            }
        }

        if local_path.is_some() && !versions.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath");
        }

        if local_path.is_none() {
            for req_kind in reqs.keys().map(|(k, _)| k) {
                if !versions.contains_key(req_kind) {
                    anyhow::bail!("missing SetVersion for {:?}", req_kind)
                }
            }
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        // If local path provided, use it directly
        if let Some(local_path) = local_path {
            ctx.emit_rust_step("use local kernel package", |ctx| {
                let local_path = local_path.claim(ctx);
                let reqs = reqs.into_values().flatten().collect::<Vec<_>>().claim(ctx);

                move |rt| {
                    let kernel_pkg_dir = rt.read(local_path);
                    // For local path, the directory itself is the package root
                    rt.write_all(reqs, &kernel_pkg_dir);
                    Ok(())
                }
            });

            return Ok(());
        }

        let extract_zip_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for ((kind, arch), out_vars) in reqs {
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
                    OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Cvm => {
                        ""
                    }
                    OpenhclKernelPackageKind::Dev | OpenhclKernelPackageKind::CvmDev => {
                        ".Dev"
                    }
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

            ctx.emit_rust_step("unpack kernel package", |ctx| {
                let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
                let out_vars = out_vars.claim(ctx);
                let kernel_package_tar_gz = kernel_package_tar_gz.claim(ctx);
                move |rt| {
                    let kernel_package_tar_gz = rt.read(kernel_package_tar_gz);

                    let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                        rt,
                        extract_zip_deps,
                        &kernel_package_tar_gz,
                        &file_name, // filename includes version and arch
                    )?;

                    let base_dir = std::env::current_dir()?;

                    if cfg!(unix) {
                        #[cfg(unix)]
                        {
                            // HACK: recreate the layout used by nuget packages.
                            let nuget_path = "build/native/bin";
                            let metadata_file = "kernel_build_metadata.json";
                            fs_err::create_dir_all(nuget_path)?;
                            fs_err::os::unix::fs::symlink(
                                extract_dir.join(metadata_file),
                                format!("{}/{}", nuget_path, metadata_file),
                            )?;

                            fs_err::os::unix::fs::symlink(
                                extract_dir,
                                format!(
                                    "{}/{}",
                                    nuget_path,
                                    match arch {
                                        OpenhclKernelPackageArch::X86_64 => "x64",
                                        OpenhclKernelPackageArch::Aarch64 => "arm64",
                                    }
                                ),
                            )?;
                        }
                    } else {
                        let _ = extract_dir;
                        anyhow::bail!(
                            "cannot download openhcl kernel package on non-unix machines"
                        );
                    }

                    rt.write_all(out_vars, &base_dir);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
