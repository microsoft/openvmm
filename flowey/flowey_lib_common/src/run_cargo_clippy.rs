// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Clippy

use crate::run_cargo_build::CargoBuildProfile;
use crate::run_cargo_build::CargoFeatureSet;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum CargoPackage {
    Workspace,
    Crate(String),
}

flowey_request! {
    pub struct Request {
        pub in_folder: ReadVar<PathBuf>,
        pub package: CargoPackage,
        pub profile: CargoBuildProfile,
        pub features: CargoFeatureSet,
        pub target: target_lexicon::Triple,
        pub extra_env: Option<Vec<(String, String)>>,
        pub exclude: ReadVar<Option<Vec<String>>>,
        pub keep_going: bool,
        pub all_targets: bool,
        /// Wait for specified side-effects to resolve before running cargo-run.
        ///
        /// (e.g: to allow for some ambient packages / dependencies to get
        /// installed).
        pub pre_build_deps: Vec<ReadVar<SideEffect>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);
        let flags = ctx.reqv(crate::cfg_cargo_common_flags::Request::GetFlags);

        for Request {
            in_folder,
            package,
            profile,
            features,
            target,
            extra_env,
            exclude,
            keep_going,
            all_targets,
            pre_build_deps,
            done,
        } in requests
        {
            ctx.req(crate::install_rust::Request::InstallTargetTriple(
                target.clone(),
            ));

            ctx.emit_rust_step("cargo clippy", |ctx| {
                pre_build_deps.claim(ctx);
                done.claim(ctx);
                let rust_toolchain = rust_toolchain.clone().claim(ctx);
                let flags = flags.clone().claim(ctx);
                let in_folder = in_folder.claim(ctx);
                let exclude = exclude.claim(ctx);
                move |rt| {
                    let rust_toolchain = rt.read(rust_toolchain);
                    let flags = rt.read(flags);
                    let in_folder = rt.read(in_folder);
                    let exclude = rt.read(exclude);

                    let crate::cfg_cargo_common_flags::Flags { locked, verbose } = flags;

                    let target = target.to_string();

                    let cargo_profile = match &profile {
                        CargoBuildProfile::Debug => "dev",
                        CargoBuildProfile::Release => "release",
                        CargoBuildProfile::Custom(s) => s,
                    };

                    let mut args = Vec::new();

                    args.push("clippy");
                    if verbose {
                        args.push("--verbose");
                    }
                    if locked {
                        args.push("--locked");
                    }
                    if keep_going {
                        args.push("--keep-going");
                    }
                    if all_targets {
                        args.push("--all-targets");
                    }
                    match &package {
                        CargoPackage::Workspace => args.push("--workspace"),
                        CargoPackage::Crate(crate_name) => {
                            args.push("-p");
                            args.push(crate_name);
                        }
                    }
                    let feature_strings = features.to_cargo_arg_strings();
                    args.extend(feature_strings.iter().map(|s| s.as_str()));
                    args.push("--target");
                    args.push(&target);
                    args.push("--profile");
                    args.push(cargo_profile);
                    if let Some(exclude) = &exclude {
                        for excluded_crate in exclude {
                            args.push("--exclude");
                            args.push(excluded_crate);
                        }
                    }

                    let sh = xshell::Shell::new()?;

                    sh.change_dir(in_folder);

                    let mut cmd = if let Some(rust_toolchain) = &rust_toolchain {
                        xshell::cmd!(sh, "rustup run {rust_toolchain} cargo")
                    } else {
                        xshell::cmd!(sh, "cargo")
                    };

                    // if running in CI, no need to waste time with incremental
                    // build artifacts
                    if !matches!(rt.backend(), FlowBackend::Local) {
                        cmd = cmd.env("CARGO_INCREMENTAL", "0");
                    }
                    if let Some(env) = extra_env {
                        for (key, val) in env {
                            log::info!("env: {key}={val}");
                            cmd = cmd.env(key, val);
                        }
                    }

                    cmd.args(args).run()?;

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
