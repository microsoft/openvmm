// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install a cached copy of `cargo-llvm-cov`.

use flowey::node::prelude::*;

flowey_config! {
    /// Configuration for the cargo-llvm-cov installer.
    pub struct Config {
        /// Version of cargo-llvm-cov to install.
        pub version: Option<String>,
    }
}

flowey_request! {
    /// Install cargo-llvm-cov as a Cargo subcommand.
    pub struct Request(pub WriteVar<SideEffect>);
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

    fn emit(config: Config, requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let version = config.version.context("missing config: version")?;
        let done: Vec<_> = requests.into_iter().map(|request| request.0).collect();
        let binary = ctx.platform().binary("cargo-llvm-cov");
        let cache_dir = ctx.emit_rust_stepv("create cargo-llvm-cov cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });
        let cache_key = ReadVar::from_static(format!(
            "cargo-llvm-cov-{version}-{}-{}",
            ctx.arch(),
            ctx.platform(),
        ));
        let hitvar = ctx.reqv(|hitvar| crate::cache::Request {
            label: "cargo-llvm-cov".into(),
            dir: cache_dir.clone(),
            key: cache_key,
            restore_keys: None,
            hitvar,
        });
        let install_root = ctx.reqv(crate::cfg_persistent_dir_cargo_install::Request);
        let toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);
        let cargo_home = ctx.reqv(crate::install_rust::Request::GetCargoHome);
        let rust_installed = ctx.reqv(crate::install_rust::Request::EnsureInstalled);
        ctx.emit_rust_step("installing cargo-llvm-cov", |ctx| {
            done.claim(ctx);
            rust_installed.claim(ctx);
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let install_root = install_root.claim(ctx);
            let toolchain = toolchain.claim(ctx);
            let cargo_home = cargo_home.claim(ctx);
            move |rt| {
                let cache_dir = rt.read(cache_dir);
                let hitvar = rt.read(hitvar);
                let install_root = rt.read(install_root);
                let toolchain = rt.read(toolchain);
                let cargo_home = rt.read(cargo_home);
                crate::_util::cargo_install::install_cached_cargo_binary(
                    rt,
                    cache_dir,
                    hitvar,
                    install_root,
                    toolchain,
                    cargo_home,
                    "cargo-llvm-cov",
                    &version,
                    &binary,
                )
            }
        });
        Ok(())
    }
}
