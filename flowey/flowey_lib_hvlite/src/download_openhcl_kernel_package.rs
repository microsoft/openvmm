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

#[derive(Serialize, Deserialize)]
pub struct OpenhclKernelPackageOutput {
    #[serde(rename = "vmlinux.dbg")]
    vmlinux_dbg: PathBuf,
}
impl Artifact for OpenhclKernelPackageOutput {}

flowey_request! {
    pub enum Request {
        /// Specify version string to use for each package kind
        Version(OpenhclKernelPackageKind, String),
        /// Download the specified kernel package
        GetPackage {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            pkg: WriteVar<PathBuf>,
        },
        /// Publish the specified kernel package as an artifact, only includes
        /// files defined in [`OpenhclKernelPackageOutput`]
        PublishBaseline {
            kind: OpenhclKernelPackageKind,
            arch: OpenhclKernelPackageArch,
            artifact: WriteVar<OpenhclKernelPackageOutput>,
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
        let mut versions: BTreeMap<OpenhclKernelPackageKind, String> = BTreeMap::new();
        let mut get_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        let mut publish_reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<OpenhclKernelPackageOutput>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::Version(arch, v) => {
                    let mut old = versions.insert(arch, v.clone());
                    same_across_all_reqs("SetVersion", &mut old, v)?
                }
                Request::GetPackage { kind, arch, pkg } => {
                    get_reqs.entry((kind, arch)).or_default().push(pkg);
                }
                Request::PublishBaseline {
                    kind,
                    arch,
                    artifact,
                } => {
                    publish_reqs.entry((kind, arch)).or_default().push(artifact);
                }
            }
        }

        for req_kind in get_reqs.keys().map(|(k, _)| k) {
            if !versions.contains_key(req_kind) {
                anyhow::bail!("missing SetVersion for {:?}", req_kind)
            }
        }

        // -- end of req processing -- //

        if get_reqs.is_empty() && publish_reqs.is_empty() {
            return Ok(());
        }

        let extract_zip_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for ((kind, arch), out_vars) in get_reqs {
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

                    println!("extract_dir: {:?}", extract_dir);
                    println!("base_dir: {:?}", base_dir);

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

        for ((kind, arch), out_vars) in publish_reqs {
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

                    let output = OpenhclKernelPackageOutput {
                        vmlinux_dbg: extract_dir.join("vmlinux.dbg"),
                    };

                    println!("vmlinux_dbg: {:?}", output.vmlinux_dbg);
                    rt.write_all(out_vars, &output);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
