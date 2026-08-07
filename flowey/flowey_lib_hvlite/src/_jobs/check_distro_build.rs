// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure `openvmm` still builds the way a Linux distribution package builds
//! it.
//!
//! This configuration does not use the repository's `.packages/`
//! provisioning. Every native dependency comes from a distribution package,
//! and the environment overrides a packager must set are set here as well.
//!
//! The build runs against the assembled source archive and unpacks it outside
//! the checkout. Building the checkout instead would let this pass on a tree a
//! packager cannot reproduce.

use crate::assemble_openvmm_source_release::SourceReleaseOutput;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub release: ReadVar<SourceReleaseOutput>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release, done } = request;

        let target = target_lexicon::triple!("x86_64-unknown-linux-gnu");

        // Do not depend on `install_openvmm_rust_build_essential`: it provisions
        // `protoc` out of `.packages/`, which is what this job exists to avoid.
        let mut deps = vec![ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled)];

        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            deps.push(
                ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![
                        "build-essential".into(),
                        "linux-libc-dev".into(),
                        "libssl-dev".into(),
                        "pkg-config".into(),
                        "protobuf-compiler".into(),
                    ],
                    done: v,
                }),
            );
        }

        ctx.req(flowey_lib_common::install_rust::Request::InstallTargetTriple(target.clone()));

        ctx.emit_rust_step("build openvmm in a distribution configuration", |ctx| {
            done.claim(ctx);
            deps.claim(ctx);
            let release = release.claim(ctx);
            move |rt| {
                let release = rt.read(release);
                let identity =
                    crate::assemble_openvmm_source_release::read_source_identity(&release.assets)?;
                let output_dir = release.assets;

                let build_root = std::env::current_dir()?.join("distro-build");
                if build_root.exists() {
                    fs_err::remove_dir_all(&build_root)?;
                }
                fs_err::create_dir_all(&build_root)?;
                let archive = output_dir.join(identity.archive_name());
                flowey::shell_cmd!(rt, "tar -xf {archive} -C {build_root}").run()?;

                let source_dir = build_root.join(identity.source_root());
                rt.sh.change_dir(&source_dir);

                // `.cargo/config.toml` does not force its `PROTOC` value, so an
                // inherited value redirects the build to the system compiler.
                let protoc = flowey::shell_cmd!(rt, "which protoc").read()?;
                let protoc = protoc.trim();

                let target = target.to_string();
                flowey::shell_cmd!(
                    rt,
                    "cargo build --release --locked -p openvmm --target {target}"
                )
                .env("PROTOC", protoc)
                .env("OPENSSL_NO_VENDOR", "1")
                // Debug info is not needed for this validation artifact and is
                // the binding constraint on runner disk.
                .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
                .env("CARGO_INCREMENTAL", "0")
                .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
