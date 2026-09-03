// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `cargo-nextest`.

use crate::cache::CacheHit;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_config! {
    /// Config for the download_cargo_nextest node.
    pub struct Config {
        /// Version of `cargo nextest` to install (e.g: "0.9.57")
        pub version: Option<String>,
    }
}

flowey_request! {
    pub enum Request {
        /// Download `cargo-nextest` as a standalone binary, without requiring Rust
        /// to be installed.
        ///
        /// Useful when running archived nextest tests in a separate job.
        Get(target_lexicon::Triple, WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut reqs: BTreeMap<String, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(target, path) => {
                    reqs.entry(target.to_string()).or_default().push(path)
                }
            }
        }

        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let reqs = reqs;

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        let cache_dir = ctx.emit_rust_stepv("create cargo-nextest cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        for (target, paths) in reqs {
            let (cache_key, cache_dir) = {
                let version = version.clone();
                let cache_key = format!("cargo-nextest-{version}-{target}");
                let cache_dir = cache_dir.map(ctx, {
                    let k = cache_key.clone();
                    |p| p.join(k)
                });
                (ReadVar::from_static(cache_key), cache_dir)
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
                let paths = paths.claim(ctx);
                let cache_dir = cache_dir.claim(ctx);
                let hitvar = hitvar.claim(ctx);

                move |rt| {
                    let cache_dir = rt.read(cache_dir);

                    let cargo_nextest_bin = if target.contains("windows") {
                        "cargo-nextest.exe"
                    } else {
                        "cargo-nextest"
                    };
                    let cached_bin_path = cache_dir.join(cargo_nextest_bin);

                    if !matches!(rt.read(hitvar), CacheHit::Hit) {
                        download_cargo_nextest(rt, version, target)?;

                        // move the downloaded bin into the cache dir
                        fs_err::create_dir_all(&cache_dir)?;
                        fs_err::rename(cargo_nextest_bin, &cached_bin_path)?;
                    }

                    let cached_bin_path = cached_bin_path.absolute()?;
                    log::info!("downloaded to {}", cached_bin_path.to_string_lossy());
                    assert!(cached_bin_path.exists());
                    for path in paths {
                        rt.write(path, &cached_bin_path);
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}

/// downloads and extracts nextest to the current dir.
/// split out to make rustfmt happy.
fn download_cargo_nextest(
    rt: &mut RustRuntimeServices<'_>,
    version: String,
    target: String,
) -> anyhow::Result<()> {
    let nextest_archive = "nextest.tar.gz";
    flowey::shell_cmd!(
        rt,
        "curl --fail -L https://get.nexte.st/{version}/{target}.tar.gz -o {nextest_archive}"
    )
    .run()?;
    flowey::shell_cmd!(rt, "tar -xf {nextest_archive}").run()?;

    Ok(())
}
