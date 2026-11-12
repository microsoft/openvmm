// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download (and optionally, install) a copy of `cargo-fuzz`.

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the download_cargo_fuzz node.
    pub struct Config {
        /// Version of `cargo fuzz` to install (e.g: "0.12.0")
        pub version: Option<String>,
    }
}

flowey_request! {
    pub enum Request {
        /// Install `cargo-fuzz` as a `cargo` extension (invoked via `cargo fuzz`).
        InstallWithCargo(WriteVar<SideEffect>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
        ctx.import::<crate::cfg_persistent_dir_cargo_install::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut install_with_cargo = Vec::new();

        for req in requests {
            match req {
                Request::InstallWithCargo(v) => install_with_cargo.push(v),
            }
        }

        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let install_with_cargo = install_with_cargo;

        // -- end of req processing -- //

        if install_with_cargo.is_empty() {
            return Ok(());
        }

        let cargo_fuzz_bin = ctx.platform().binary("cargo-fuzz");

        let cache_dir = ctx.emit_rust_stepv("create cargo-fuzz cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let cache_key = ReadVar::from_static(format!(
            "cargo-fuzz-{version}-{}-{}",
            ctx.arch(),
            ctx.platform()
        ));
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
                let hitvar = rt.read(hitvar);
                let cargo_install_persistent_dir = rt.read(cargo_install_persistent_dir);
                let rust_toolchain = rt.read(rust_toolchain);
                let cargo_home = rt.read(cargo_home);

                crate::_util::cargo_install::install_cached_cargo_binary(
                    rt,
                    cache_dir,
                    hitvar,
                    cargo_install_persistent_dir,
                    rust_toolchain,
                    cargo_home,
                    "cargo-fuzz",
                    &version,
                    &cargo_fuzz_bin,
                )
            }
        });

        Ok(())
    }
}
