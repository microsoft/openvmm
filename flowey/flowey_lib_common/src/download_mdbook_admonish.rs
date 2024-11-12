// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `mdbook-admonish`

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `mdbook-admonish` to install
        Version(String),
        /// Get a path to `mdbook-admonish`
        GetMdbookAdmonish(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::download_gh_release::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut get_mdbook_admonish = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::GetMdbookAdmonish(v) => get_mdbook_admonish.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let get_mdbook_admonish = get_mdbook_admonish;

        // -- end of req processing -- //

        if get_mdbook_admonish.is_empty() {
            return Ok(());
        }

        let mdbook_admonish_bin = ctx.platform().binary("mdbook-admonish");

        let tag = format!("v{version}");
        let file_name = format!(
            "mdbook-admonish-v{}-x86_64-{}",
            version,
            match ctx.platform() {
                FlowPlatform::Windows => "pc-windows-msvc.zip",
                FlowPlatform::Linux(_) => "unknown-linux-gnu.tar.gz",
                FlowPlatform::MacOs => "apple-darwin.tar.gz",
                platform => anyhow::bail!("unsupported platform {platform}"),
            }
        );

        let mdbook_zip = ctx.reqv(|v| crate::download_gh_release::Request {
            repo_owner: "tommilligan".into(),
            repo_name: "mdbook-admonish".into(),
            needs_auth: false,
            tag: tag.clone(),
            file_name: file_name.clone(),
            path: v,
        });

        let extract_zip_deps = crate::_util::extract::extract_zip_if_new_deps(ctx);
        ctx.emit_rust_step("unpack mdbook-admonish", |ctx| {
            let extract_zip_deps = extract_zip_deps.clone().claim(ctx);
            let get_mdbook_admonish = get_mdbook_admonish.claim(ctx);
            let mdbook_zip = mdbook_zip.claim(ctx);
            move |rt| {
                let mdbook_zip = rt.read(mdbook_zip);

                let extract_dir = crate::_util::extract::extract_zip_if_new(
                    rt,
                    extract_zip_deps,
                    &mdbook_zip,
                    &tag,
                )?;

                let mdbook_admonish_bin = extract_dir.join(mdbook_admonish_bin);

                rt.write_all(get_mdbook_admonish, &mdbook_admonish_bin);

                Ok(())
            }
        });

        Ok(())
    }
}
