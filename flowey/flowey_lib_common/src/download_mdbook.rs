// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `mdbook`

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the download_mdbook node.
    pub struct Config {
        /// Version of `mdbook` to install
        pub version: Option<String>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get a path to `mdbook`
        GetMdbook(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let (version, get_mdbook) =
            collect_download_requests(config.version, requests, |Request::GetMdbook(v)| v)?;

        if get_mdbook.is_empty() {
            return Ok(());
        }

        download_mdbook_tool(ctx, "mdbook", "rust-lang", "mdBook", &version, get_mdbook)
    }
}

pub(crate) fn collect_download_requests<T>(
    version: Option<String>,
    requests: Vec<T>,
    get_path: impl FnMut(T) -> WriteVar<PathBuf>,
) -> anyhow::Result<(String, Vec<WriteVar<PathBuf>>)> {
    let version = version.ok_or(anyhow::anyhow!("missing config: version"))?;
    let paths = requests.into_iter().map(get_path).collect();
    Ok((version, paths))
}

pub(crate) fn download_mdbook_tool(
    ctx: &mut NodeCtx<'_>,
    name: &str,
    repo_owner: &str,
    repo_name: &str,
    version: &str,
    paths: Vec<WriteVar<PathBuf>>,
) -> anyhow::Result<()> {
    let binary = ctx.platform().binary(name);
    let tag = format!("v{version}");
    let file_name = format!(
        "{name}-v{version}-x86_64-{}",
        match ctx.platform() {
            FlowPlatform::Windows => "pc-windows-msvc.zip",
            FlowPlatform::Linux(_) => "unknown-linux-gnu.tar.gz",
            FlowPlatform::MacOs => "apple-darwin.tar.gz",
            platform => anyhow::bail!("unsupported platform {platform}"),
        }
    );

    let archive = ctx.reqv(|v| crate::download_gh_release::Request {
        repo_owner: repo_owner.into(),
        repo_name: repo_name.into(),
        needs_auth: false,
        tag: tag.clone(),
        file_name,
        path: v,
    });

    let extract_zip_deps = crate::_util::extract::extract_zip_if_new_deps(ctx);
    ctx.emit_rust_step(format!("unpack {name}"), |ctx| {
        let extract_zip_deps = extract_zip_deps.claim(ctx);
        let paths = paths.claim(ctx);
        let archive = archive.claim(ctx);
        move |rt| {
            let archive = rt.read(archive);

            let extract_dir =
                crate::_util::extract::extract_zip_if_new(rt, extract_zip_deps, &archive, &tag)?;

            rt.write_all(paths, &extract_dir.join(binary));

            Ok(())
        }
    });
    Ok(())
}
