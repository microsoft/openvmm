// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download (and optionally, install) a copy of `cargo-fuzz`.

use crate::cache::CacheHit;
use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `cargo fuzz` to install (e.g: "0.12.0")
        Version(String),
        /// Install `cargo-fuzz` as a `cargo` extension (invoked via `cargo fuzz`).
        InstallWithCargo(WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
        ctx.import::<crate::cfg_persistent_dir_cargo_install::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut install_with_cargo = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::InstallWithCargo(v) => install_with_cargo.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let install_with_cargo = install_with_cargo;

        // -- end of req processing -- //

        if install_with_cargo.is_empty() {
            return Ok(());
        }

        let cargo_fuzz_bin = ctx.platform().binary("cargo-fuzz");

        let cache_dir = ctx.emit_rust_stepv("create cargo-fuzz cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let cache_key = ReadVar::from_static(format!("cargo-fuzz-{version}"));
        let hitvar = ctx.reqv(|v| {
            crate::cache::Request {
                label: "cargo-fuzz".into(),
                dir: cache_dir.clone(),
                key: cache_key,
                restore_keys: None, // we want an exact hit
                hitvar: v,
            }
        });

        let cargo_install_persistent_dir =
            ctx.reqv(crate::cfg_persistent_dir_cargo_install::Request);
        let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);
        let cargo_home = ctx.reqv(crate::install_rust::Request::GetCargoHome);

        ctx.emit_rust_step("installing cargo-fuzz", |ctx| {
            install_with_cargo.claim(ctx);

            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let cargo_install_persistent_dir = cargo_install_persistent_dir.claim(ctx);
            let rust_toolchain = rust_toolchain.claim(ctx);
            let cargo_home = cargo_home.claim(ctx);

            move |rt| {
                let cache_dir = rt.read(cache_dir);

                let cached_bin_path = cache_dir.join(&cargo_fuzz_bin);
                let cached = if matches!(rt.read(hitvar), CacheHit::Hit) {
                    assert!(cached_bin_path.exists());
                    Some(cached_bin_path.clone())
                } else {
                    None
                };

                let path_to_cargo_fuzz = if let Some(cached) = cached {
                    cached
                } else {
                    let root = rt.read(cargo_install_persistent_dir).unwrap_or("./".into());

                    let rust_toolchain = rt.read(rust_toolchain);
                    let run = |offline| {
                        let rust_toolchain = rust_toolchain.as_ref().map(|s| format!("+{s}"));

                        flowey::shell_cmd!(
                            rt,
                            "cargo {rust_toolchain...}
                                install
                                --locked
                                {offline...}
                                --root {root}
                                --target-dir {root}
                                --version {version}
                                cargo-fuzz
                            "
                        )
                        .run()
                    };

                    // Try --offline to avoid an unnecessary git fetch on rerun.
                    if run(Some("--offline")).is_err() {
                        // Try again without --offline.
                        run(None)?;
                    }

                    let out_bin = root.absolute()?.join("bin").join(&cargo_fuzz_bin);

                    // move the compiled bin into the cache dir
                    fs_err::rename(out_bin, &cached_bin_path)?;
                    cached_bin_path.absolute()?
                };

                // is installing with cargo, make sure the bin we built /
                // downloaded is accessible via cargo fuzz
                fs_err::copy(
                    &path_to_cargo_fuzz,
                    rt.read(cargo_home).join("bin").join(&cargo_fuzz_bin),
                )?;

                Ok(())
            }
        });

        Ok(())
    }
}
