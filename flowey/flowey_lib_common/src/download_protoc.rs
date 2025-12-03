// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `protoc` for the current platform

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct ProtocPackage {
    pub protoc_bin: PathBuf,
    pub include_dir: PathBuf,
}

flowey_request! {
    pub enum Request {
        /// Use a locally downloaded protoc
        LocalPath(PathBuf),
        /// What version to download (e.g: 27.1)
        Version(String),
        /// Return paths to items in the protoc package
        Get(WriteVar<ProtocPackage>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::download_gh_release::Node>();
        ctx.import::<crate::cache::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut local_path = None;
        let mut get_reqs = Vec::new();

        for req in requests {
            match req {
                Request::LocalPath(path) => {
                    same_across_all_reqs("LocalPath", &mut local_path, path)?
                }
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::Get(v) => get_reqs.push(v),
            }
        }

        if version.is_some() && local_path.is_some() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_path.is_none() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if get_reqs.is_empty() {
            return Ok(());
        }

        if let Some(local_path) = local_path {
            ctx.emit_rust_step("use local protoc", |ctx| {
                let get_reqs = get_reqs.claim(ctx);
                let local_path = local_path.clone();
                move |rt| {
                    let protoc_bin = local_path
                        .join("bin")
                        .join(rt.platform().binary("protoc"))
                        .absolute()?;

                    assert!(protoc_bin.exists());

                    // Don't try to make executable - local paths (especially from nix store)
                    // should already be executable and may be read-only

                    let protoc_includes = local_path.join("include").absolute()?;
                    assert!(protoc_includes.exists());

                    let pkg = ProtocPackage {
                        protoc_bin,
                        include_dir: protoc_includes,
                    };

                    rt.write_all(get_reqs, &pkg);

                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local requests handled above");

        let tag = format!("v{version}");
        let file_name = format!(
            "protoc-{}-{}.zip",
            version,
            match (ctx.platform(), ctx.arch()) {
                // protoc is not currently available for windows aarch64,
                // so emulate the x64 version
                (FlowPlatform::Windows, _) => "win64",
                (FlowPlatform::Linux(_), FlowArch::X86_64) => "linux-x86_64",
                (FlowPlatform::Linux(_), FlowArch::Aarch64) => "linux-aarch_64",
                (FlowPlatform::MacOs, FlowArch::X86_64) => "osx-x86_64",
                (FlowPlatform::MacOs, FlowArch::Aarch64) => "osx-aarch_64",
                (platform, arch) => anyhow::bail!("unsupported platform {platform} {arch}"),
            }
        );

        let protoc_zip = ctx.reqv(|v| crate::download_gh_release::Request {
            repo_owner: "protocolbuffers".into(),
            repo_name: "protobuf".into(),
            needs_auth: false,
            tag: tag.clone(),
            file_name: file_name.clone(),
            path: v,
        });

        let extract_zip_deps = crate::_util::extract::extract_zip_if_new_deps(ctx);
        ctx.emit_rust_step("unpack protoc", |ctx| {
            let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
            let get_reqs = get_reqs.claim(ctx);
            let protoc_zip = protoc_zip.claim(ctx);
            move |rt| {
                let protoc_zip = rt.read(protoc_zip);

                let extract_dir = crate::_util::extract::extract_zip_if_new(
                    rt,
                    extract_zip_deps,
                    &protoc_zip,
                    &tag,
                )?;

                let protoc_bin = extract_dir
                    .join("bin")
                    .join(rt.platform().binary("protoc"))
                    .absolute()?;

                assert!(protoc_bin.exists());

                protoc_bin.make_executable()?;

                let protoc_includes = extract_dir.join("include").absolute()?;
                assert!(protoc_includes.exists());

                let pkg = ProtocPackage {
                    protoc_bin,
                    include_dir: protoc_includes,
                };

                rt.write_all(get_reqs, &pkg);

                Ok(())
            }
        });

        Ok(())
    }
}
