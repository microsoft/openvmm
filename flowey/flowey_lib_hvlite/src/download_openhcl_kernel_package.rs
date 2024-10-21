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
        ctx.import::<flowey_lib_common::install_apt_pkg::Node>();
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut versions: BTreeMap<OpenhclKernelPackageKind, String> = BTreeMap::new();
        let mut reqs: BTreeMap<
            (OpenhclKernelPackageKind, OpenhclKernelPackageArch),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::Version(arch, v) => {
                    let mut old = versions.insert(arch, v.clone());
                    same_across_all_reqs("SetVersion", &mut old, v)?
                }
                Request::GetPackage { kind, arch, pkg } => {
                    reqs.entry((kind, arch)).or_default().push(pkg)
                }
            }
        }

        for req_kind in reqs.keys().map(|(k, _)| k) {
            if !versions.contains_key(req_kind) {
                anyhow::bail!("missing SetVersion for {:?}", req_kind)
            }
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        let extract_archive_deps =
            flowey_lib_common::_util::extract::extract_archive_if_new_deps(ctx);

        for ((kind, arch), out_vars) in reqs {
            let version = versions.get(&kind).expect("checked above");
            let kind_string = match kind {
                OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Cvm => "main",
                OpenhclKernelPackageKind::Dev | OpenhclKernelPackageKind::CvmDev => "dev",
            };

            let tag = format!("rolling-lts/hcl-{kind_string}/{version}");

            let file_name = format!(
                "Microsoft.OHCL.Kernel.{}-{}-{}.zip",
                version,
                match kind {
                    OpenhclKernelPackageKind::Main | OpenhclKernelPackageKind::Dev => {
                        kind_string.to_string()
                    }
                    OpenhclKernelPackageKind::Cvm | OpenhclKernelPackageKind::CvmDev => {
                        format!("{kind_string}-cvm")
                    }
                },
                match arch {
                    OpenhclKernelPackageArch::X86_64 => "x64",
                    OpenhclKernelPackageArch::Aarch64 => "arm64",
                }
            );

            let kernel_package_zip =
                ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "OHCL-Linux-Kernel".into(),
                    tag,
                    file_name: file_name.clone(),
                    path: v,
                });

            ctx.emit_rust_step(format!("unpack {file_name}"), |ctx| {
                let extract_archive_deps = extract_archive_deps.clone().claim(ctx);
                let out_vars = out_vars.claim(ctx);
                let kernel_package_zip = kernel_package_zip.claim(ctx);
                move |rt| {
                    let kernel_package_zip = rt.read(kernel_package_zip);

                    let extract_dir = flowey_lib_common::_util::extract::extract_archive_if_new(
                        rt,
                        extract_archive_deps,
                        &kernel_package_zip,
                        &file_name, // filename currently includes version and arch
                    )?;

                    let base_dir = std::env::current_dir()?;

                    if cfg!(unix) {
                        #[cfg(unix)]
                        {
                            // HACK: recursively chmod all the files, otherwise they all have 000 access.
                            let sh = xshell::Shell::new()?;
                            xshell::cmd!(sh, "chmod -R +rwX {extract_dir}").run()?;

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
