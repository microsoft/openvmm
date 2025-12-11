// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built mu_msvm package from its GitHub Release.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MuMsvmArch {
    X86_64,
    Aarch64,
}

flowey_request! {
    pub enum Request {
        /// Use a locally provided UEFI firmware file
        LocalPath(ReadVar<PathBuf>),
        /// Specify version of mu_msvm to use
        Version(String),
        /// Download the mu_msvm package for the given arch
        GetMsvmFd {
            arch: MuMsvmArch,
            msvm_fd: WriteVar<PathBuf>
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
        let mut version = None;
        let mut local_path: Option<ReadVar<PathBuf>> = None;
        let mut reqs: BTreeMap<MuMsvmArch, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::LocalPath(path) => {
                    same_across_all_reqs_backing_var("LocalPath", &mut local_path, path)?
                }
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::GetMsvmFd { arch, msvm_fd } => reqs.entry(arch).or_default().push(msvm_fd),
            }
        }

        if version.is_some() && local_path.is_some() {
            anyhow::bail!("Cannot specify both Version and LocalPath");
        }

        if version.is_none() && local_path.is_none() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        // If local path provided, use it directly
        if let Some(local_path) = local_path {
            ctx.emit_rust_step("use local UEFI firmware", |ctx| {
                let local_path = local_path.claim(ctx);
                let reqs = reqs.into_values().flatten().collect::<Vec<_>>().claim(ctx);

                move |rt| {
                    let msvm_fd = rt.read(local_path);
                    rt.write_all(reqs, &msvm_fd);
                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("checked above");

        let extract_zip_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for (arch, out_vars) in reqs {
            let file_name = match arch {
                MuMsvmArch::X86_64 => "RELEASE-X64-artifacts.zip",
                MuMsvmArch::Aarch64 => "RELEASE-AARCH64-artifacts.zip",
            };

            let mu_msvm_zip = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "mu_msvm".into(),
                needs_auth: false,
                tag: format!("v{version}"),
                file_name: file_name.into(),
                path: v,
            });

            let zip_file_version = format!("{version}-{file_name}");

            ctx.emit_rust_step(
                {
                    format!(
                        "unpack mu_msvm package ({})",
                        match arch {
                            MuMsvmArch::X86_64 => "x64",
                            MuMsvmArch::Aarch64 => "aarch64",
                        },
                    )
                },
                |ctx| {
                    let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
                    let out_vars = out_vars.claim(ctx);
                    let mu_msvm_zip = mu_msvm_zip.claim(ctx);
                    move |rt| {
                        let mu_msvm_zip = rt.read(mu_msvm_zip);

                        let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                            rt,
                            extract_zip_deps,
                            &mu_msvm_zip,
                            &zip_file_version,
                        )?;

                        let msvm_fd = extract_dir.join("FV/MSVM.fd");

                        for var in out_vars {
                            rt.write(var, &msvm_fd)
                        }

                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}
