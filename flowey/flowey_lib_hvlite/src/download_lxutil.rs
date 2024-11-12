// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built lxutil package from its GitHub Release.
//!
//! NOTE: In the future, `lxutil` will be rewritten in Rust, and downloading a
//! pre-compiled dll will no longer be necessary.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct LxutilPackage {
    pub lxutil_dll: PathBuf,
    pub lxutil_pdb: PathBuf,
    pub lxutil_lib: PathBuf,
    pub lxutil_h: PathBuf,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LxutilArch {
    X86_64,
    Aarch64,
}

flowey_request! {
    pub enum Request {
        /// Specify version of lxutil to use
        Version(String),
        /// Download the lxutil package for the given arch
        GetPackage {
            arch: LxutilArch,
            pkg: WriteVar<LxutilPackage>
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
        let mut reqs: BTreeMap<LxutilArch, Vec<WriteVar<LxutilPackage>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::GetPackage { arch, pkg } => reqs.entry(arch).or_default().push(pkg),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        let extract_zip_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);

        for (arch, out_vars) in reqs {
            let tag = format!("Microsoft.WSL.LxUtil.{}", version);

            let file_name = {
                let arch_str = match arch {
                    LxutilArch::X86_64 => "x64",
                    LxutilArch::Aarch64 => "AARCH64",
                };
                format!("Microsoft.WSL.LxUtil.{}.zip", arch_str)
            };

            // NOTE: this release is part of the openvmm-deps repo, simply
            // because that's a convenient repo to hang host this artifact.
            let lxutil_zip = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: tag.clone(),
                file_name: file_name.clone(),
                path: v,
            });

            ctx.emit_rust_step(format!("unpack {}", file_name), |ctx| {
                let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
                let out_vars = out_vars.claim(ctx);
                let lxutil_zip = lxutil_zip.claim(ctx);
                move |rt| {
                    let lxutil_zip = rt.read(lxutil_zip);

                    let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                        rt,
                        extract_zip_deps,
                        &lxutil_zip,
                        &tag,
                    )?;

                    let lxutil_package = LxutilPackage {
                        lxutil_dll: extract_dir.join("native/bin/lxutil.dll"),
                        lxutil_pdb: extract_dir.join("native/bin/lxutil.pdb"),
                        lxutil_lib: extract_dir.join("native/lib/lxutil.lib"),
                        lxutil_h: extract_dir.join("native/include/lxutil.h"),
                    };

                    rt.write_all(out_vars, &lxutil_package);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
