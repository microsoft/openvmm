// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `cargo-nextest`.

use crate::cache::CacheHit;
use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `cargo nextest` to install (e.g: "0.9.57")
        Version(String),
        /// Download `cargo-nextest` as a standalone binary, without requiring Rust
        /// to be installed.
        ///
        /// Useful when running archived nextest tests in a separate job.
        Get(ReadVar<target_lexicon::Triple>, WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut reqs = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::Get(target, path) => reqs.push((target, path)),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let reqs = reqs;

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        let cache_dir = ctx.emit_rust_stepv("create cargo-nextest cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        for (target, path) in reqs {
            let (cache_key, cache_dir) = {
                let version = version.clone();
                let cache_key = target.map(ctx, move |target| {
                    format!("cargo-nextest-{version}-{target}")
                });
                let cache_dir = cache_dir
                    .zip(ctx, cache_key.clone())
                    .map(ctx, |(p, k)| p.join(k));
                (cache_key, cache_dir)
            };

            let hitvar = ctx.reqv(|v| {
                crate::cache::Request {
                    label: "cargo-nextest".into(),
                    dir: cache_dir.clone(),
                    key: cache_key,
                    restore_keys: None, // we want an exact hit
                    hitvar: v,
                }
            });

            let version = version.clone();
            ctx.emit_rust_step("downloading cargo-nextest", |ctx| {
                let path = path.claim(ctx);
                let cache_dir = cache_dir.claim(ctx);
                let hitvar = hitvar.claim(ctx);
                let target = target.claim(ctx);

                move |rt| {
                    let cache_dir = rt.read(cache_dir);
                    let target = rt.read(target);

                    let cargo_nextest_bin = match target.operating_system {
                        target_lexicon::OperatingSystem::Windows => "cargo-nextest.exe",
                        _ => "cargo-nextest",
                    };
                    let cached_bin_path = cache_dir.join(cargo_nextest_bin);
                    let target = target.to_string();

                    if !matches!(rt.read(hitvar), CacheHit::Hit) {
                        let nextest_archive = "nextest.tar.gz";
                        flowey::shell_cmd!(rt, "curl --fail -L https://get.nexte.st/{version}/{target}.tar.gz -o {nextest_archive}").run()?;
                        flowey::shell_cmd!(rt, "tar -xf {nextest_archive}").run()?;

                        // move the downloaded bin into the cache dir
                        fs_err::create_dir_all(&cache_dir)?;
                        fs_err::rename(cargo_nextest_bin, &cached_bin_path)?;
                    }

                    let cached_bin_path = cached_bin_path.absolute()?;
                    log::info!("downloaded to {}", cached_bin_path.to_string_lossy());
                    assert!(cached_bin_path.exists());
                    rt.write(path, &cached_bin_path);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
