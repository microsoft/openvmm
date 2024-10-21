// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download (and optionally, install) a copy of `cargo-nextest`.

use crate::cache::CacheHit;
use crate::cache::CacheResult;
use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `cargo nextest` to install (e.g: "0.9.57")
        Version(String),
        /// Install `cargo-nextest` as a `cargo` extension (invoked via `cargo
        /// nextest`).
        InstallWithCargo(WriteVar<SideEffect>),
        /// Install `cargo-nextest` as a standalone binary, without requiring Rust
        /// to be installed.
        ///
        /// Useful when running archived nextest tests in a separate job.
        InstallStandalone(WriteVar<PathBuf>),
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
        let mut install_standalone = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::InstallWithCargo(v) => install_with_cargo.push(v),
                Request::InstallStandalone(v) => install_standalone.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let install_with_cargo = install_with_cargo;
        let install_standalone = install_standalone;

        // -- end of req processing -- //

        if install_standalone.is_empty() && install_with_cargo.is_empty() {
            return Ok(());
        }

        let cargo_nextest_bin = ctx.platform().binary("cargo-nextest");

        let cache_dir = ctx.emit_rust_stepv("create cargo-nextest cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let cache_key = ReadVar::from_static(format!("cargo-nextest-{version}"));
        let hitvar = ctx.reqv(|v| {
            crate::cache::Request {
                label: "cargo-nextest".into(),
                dir: cache_dir.clone(),
                key: cache_key,
                restore_keys: None, // we want an exact hit
                hitvar: CacheResult::HitVar(v),
            }
        });

        let rust_deps = if !install_with_cargo.is_empty() {
            // in case we end up doing a cargo-install
            let cargo_install_persistent_dir =
                ctx.reqv(crate::cfg_persistent_dir_cargo_install::Request);

            let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);

            let cargo_home = ctx.reqv(crate::install_rust::Request::GetCargoHome);

            Some((cargo_install_persistent_dir, rust_toolchain, cargo_home))
        } else {
            None
        };

        ctx.emit_rust_step("installing cargo-nextest", |ctx| {
            install_with_cargo.claim(ctx);

            let install_standalone = install_standalone.claim(ctx);
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let rust_deps = rust_deps.map(|(a, b, c)| (a.claim(ctx), b.claim(ctx), c.claim(ctx)));

            move |rt| {
                let cache_dir = rt.read(cache_dir);
                let rust_deps = rust_deps.map(|(a, b, c)| (rt.read(a), rt.read(b), rt.read(c)));

                let cached_bin_path = cache_dir.join(&cargo_nextest_bin);
                let cached = if matches!(rt.read(hitvar), CacheHit::Hit) {
                    assert!(cached_bin_path.exists());
                    Some(cached_bin_path.clone())
                } else {
                    None
                };

                let (cargo_home, path_to_cargo_nextest) = if let Some(cached) = cached {
                    (rust_deps.map(|(_, _, cargo_home)| cargo_home), cached)
                } else if let Some((cargo_install_persistent_dir, rust_toolchain, cargo_home)) =
                    rust_deps
                {
                    let root = cargo_install_persistent_dir.unwrap_or("./".into());

                    let sh = xshell::Shell::new()?;
                    let run = |offline| {
                        let rust_toolchain = rust_toolchain.as_ref();
                        let rust_toolchain = rust_toolchain.map(|s| format!("+{s}"));

                        xshell::cmd!(
                            sh,
                            "cargo {rust_toolchain...}
                                install
                                --locked
                                {offline...}
                                --root {root}
                                --target-dir {root}
                                --version {version}
                                cargo-nextest
                            "
                        )
                        .run()
                    };

                    // Try --offline to avoid an unnecessary git fetch on rerun.
                    if run(Some("--offline")).is_err() {
                        // Try again without --offline.
                        run(None)?;
                    }

                    let out_bin = root.absolute()?.join("bin").join(&cargo_nextest_bin);

                    // move the compiled bin into the cache dir
                    fs_err::rename(out_bin, &cached_bin_path)?;
                    let final_bin = cached_bin_path.absolute()?;

                    (Some(cargo_home), final_bin)
                } else {
                    log::error!(
                        "specified standalone installation, but not standalone bin could be found!"
                    );
                    anyhow::bail!("could not install cargo-nextest")
                };

                // is installing with cargo, make sure the bin we built /
                // downloaded is accessible via cargo nextest
                if let Some(cargo_home) = cargo_home {
                    fs_err::copy(
                        &path_to_cargo_nextest,
                        cargo_home.join("bin").join(&cargo_nextest_bin),
                    )?;
                }

                for var in install_standalone {
                    rt.write(var, &path_to_cargo_nextest)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
