// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of the GitHub CLI.
//!
//! NOTE: this node will _not_ set up any form of authentication for the
//! downloaded CLI binary!

use crate::cache::CacheHit;
use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `gh` to download (e.g: 2.52.0)
        Version(String),
        /// Get a path to downloaded `gh`
        Get(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::cache::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut install_reqs = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::Get(v) => install_reqs.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let install_reqs = install_reqs;

        // -- end of req processing -- //

        if install_reqs.is_empty() {
            return Ok(());
        }

        let gh_bin = ctx.platform().binary("gh");

        let gh_arch = match ctx.arch() {
            FlowArch::X86_64 => "amd64",
            FlowArch::Aarch64 => "arm64",
            arch => anyhow::bail!("unsupported architecture {arch}"),
        };

        let cache_dir = ctx.emit_rust_stepv("create gh cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let cache_key = ReadVar::from_static(format!("gh-cli-{version}"));
        let hitvar = ctx.reqv(|hitvar| crate::cache::Request {
            label: "gh-cli".into(),
            dir: cache_dir.clone(),
            key: cache_key,
            restore_keys: None,
            hitvar,
        });

        ctx.emit_rust_step("installing gh", |ctx| {
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let install_reqs = install_reqs.claim(ctx);
            move |rt| {
                let cache_dir = rt.read(cache_dir);

                let cached = if matches!(rt.read(hitvar), CacheHit::Hit) {
                    let cached_bin = cache_dir.join(&gh_bin);
                    assert!(cached_bin.exists());
                    Some(cached_bin)
                } else {
                    None
                };

                let path_to_gh = if let Some(cached) = cached {
                    cached
                } else {
                    match rt.platform() {
                        FlowPlatform::Windows => {
                            flowey::shell_cmd!(rt, "curl --fail -L https://github.com/cli/cli/releases/download/v{version}/gh_{version}_windows_{gh_arch}.zip -o gh.zip").run()?;
                            flowey::shell_cmd!(rt, "tar -xf gh.zip").run()?;
                        },
                        FlowPlatform::Linux(_) => {
                            flowey::shell_cmd!(rt, "curl --fail -L https://github.com/cli/cli/releases/download/v{version}/gh_{version}_linux_{gh_arch}.tar.gz -o gh.tar.gz").run()?;
                            flowey::shell_cmd!(rt, "tar -xf gh.tar.gz --strip-components=1").run()?;
                        },
                        FlowPlatform::MacOs => {
                            flowey::shell_cmd!(rt, "curl --fail -L https://github.com/cli/cli/releases/download/v{version}/gh_{version}_macOS_{gh_arch}.zip -o gh.zip").run()?;
                            flowey::shell_cmd!(rt, "tar -xf gh.zip --strip-components=1").run()?;
                        }
                        platform => anyhow::bail!("unsupported platform {platform}"),
                    };

                    // move the unzipped bin into the cache dir
                    let final_bin = cache_dir.join(&gh_bin);
                    fs_err::rename(format!("bin/{gh_bin}"), &final_bin)?;

                    final_bin.absolute()?
                };

                for var in install_reqs {
                    rt.write(var, &path_to_gh)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
