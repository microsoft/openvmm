// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and archive cargo-nextest tests (for cross-job execution).
//!
//! Respects common cargo flags specified by the `cfg_cargo_common_flags` node.

use crate::run_cargo_nextest_run::build_params::NextestBuildParams;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Friendly label for this request that shows up in logs.
        pub friendly_label: String,
        /// Directory to run `cargo nextest archive` within
        pub working_dir: ReadVar<PathBuf>,
        /// Build parameters to use when compiling the tests.
        pub build_params: NextestBuildParams,
        /// Wait for specified side-effects to resolve before building / running any
        /// tests. (e.g: to allow for some ambient packages / dependencies to
        /// get installed).
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        /// Resulting nextest archive file
        pub archive_file: WriteVar<PathBuf>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::install_cargo_nextest::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let cargo_flags = ctx.reqv(crate::cfg_cargo_common_flags::Request::GetFlags);

        let nextest_installed = ctx.reqv(crate::install_cargo_nextest::Request);

        let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);

        for Request {
            friendly_label,
            working_dir,
            build_params:
                NextestBuildParams {
                    packages,
                    features,
                    no_default_features,
                    target,
                    profile,
                    extra_env,
                },
            pre_run_deps,
            archive_file,
        } in requests
        {
            ctx.req(crate::install_rust::Request::InstallTargetTriple(
                target.clone(),
            ));

            ctx.emit_rust_step(
                format!("build + archive '{friendly_label}' nextests"),
                |ctx| {
                    pre_run_deps.claim(ctx);
                    nextest_installed.clone().claim(ctx);
                    let cargo_flags = cargo_flags.clone().claim(ctx);
                    let rust_toolchain = rust_toolchain.clone().claim(ctx);
                    let working_dir = working_dir.claim(ctx);
                    let archive_file = archive_file.claim(ctx);
                    let packages = packages.claim(ctx);
                    let extra_env = extra_env.claim(ctx);
                    move |rt| {
                        let cargo_flags = rt.read(cargo_flags);
                        let working_dir = rt.read(working_dir);
                        let rust_toolchain = rt.read(rust_toolchain);
                        let packages = rt.read(packages);
                        let extra_env = rt.read(extra_env);

                        let rust_toolchain = rust_toolchain.map(|s| format!("+{s}"));
                        let (build_args, build_env) =
                            crate::gen_cargo_nextest_run_cmd::cargo_nextest_build_args_and_env(
                                cargo_flags,
                                profile,
                                target,
                                packages,
                                features,
                                no_default_features,
                                extra_env,
                            );

                        let sh = xshell::Shell::new()?;

                        let out_archive_file = sh.current_dir().absolute()?.join("archive.tar.zst");

                        sh.change_dir(working_dir);
                        let mut cmd = xshell::cmd!(
                            sh,
                            "cargo {rust_toolchain...} nextest archive
                                {build_args...}
                                --archive-file {out_archive_file}
                            "
                        );

                        // if running in CI, no need to waste time with incremental
                        // build artifacts
                        if !matches!(rt.backend(), FlowBackend::Local) {
                            cmd = cmd.env("CARGO_INCREMENTAL", "0");
                        }

                        for (k, v) in build_env {
                            cmd = cmd.env(k, v);
                        }

                        cmd.run()?;

                        rt.write(archive_file, &out_archive_file);

                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}
