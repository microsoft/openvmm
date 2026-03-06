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
        /// Specify version of mu_msvm to use
        Version(String),
        /// Use a local MSVM.fd path for a specific architecture
        LocalPath(MuMsvmArch, ReadVar<PathBuf>),
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
        let mut local_paths: BTreeMap<MuMsvmArch, ReadVar<PathBuf>> = BTreeMap::new();
        let mut reqs: BTreeMap<MuMsvmArch, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::LocalPath(arch, path) => {
                    if local_paths.contains_key(&arch) {
                        anyhow::bail!("Duplicate LocalPath requests for {:?}", arch,);
                    }
                    local_paths.insert(arch, path);
                }
                Request::GetMsvmFd { arch, msvm_fd } => reqs.entry(arch).or_default().push(msvm_fd),
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local mu_msvm UEFI", |ctx| {
                let reqs = reqs.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(arch, var)| (arch, var.claim(ctx)))
                    .collect();
                move |rt| {
                    for (arch, out_vars) in reqs {
                        let msvm_fd_var = local_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for architecture {:?}", arch)
                        })?;
                        let msvm_fd = rt.read(msvm_fd_var.clone());
                        for var in out_vars {
                            log::info!(
                                "using local uefi for {} at path {:?}",
                                match arch {
                                    MuMsvmArch::X86_64 => "x64",
                                    MuMsvmArch::Aarch64 => "aarch64",
                                },
                                msvm_fd
                            );
                            rt.write(var, &msvm_fd);
                        }
                    }
                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local paths handled above");
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
