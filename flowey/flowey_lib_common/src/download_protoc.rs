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
        let mut get_reqs = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::Get(v) => get_reqs.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;

        // -- end of req processing -- //

        if get_reqs.is_empty() {
            return Ok(());
        }

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

                // Make sure protoc is executable
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let old_mode = protoc_bin.metadata()?.permissions().mode();
                    fs_err::set_permissions(
                        &protoc_bin,
                        std::fs::Permissions::from_mode(old_mode | 0o111),
                    )?;
                }

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
