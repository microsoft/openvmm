// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check every OpenVMM workspace feature with `cargo-hack`.

use crate::common::CommonProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub profile: CommonProfile,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<flowey_lib_common::install_cargo_hack::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { profile, done } = request;

        let mut pre_build_deps = vec![
            ctx.reqv(crate::install_openvmm_rust_build_essential::Request),
            ctx.reqv(flowey_lib_common::install_cargo_hack::Request),
        ];

        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            pre_build_deps.push(ctx.reqv(|done| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec!["libssl-dev".into(), "pkg-config".into()],
                    done,
                }
            }));
        }

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let rust_toolchain = ctx.reqv(flowey_lib_common::install_rust::Request::GetRustupToolchain);

        ctx.emit_rust_step("run cargo hack", |ctx| {
            done.claim(ctx);
            pre_build_deps.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let rust_toolchain = rust_toolchain.claim(ctx);
            move |rt| {
                let openvmm_repo_path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(openvmm_repo_path);

                let rust_toolchain = rt
                    .read(rust_toolchain)
                    .as_ref()
                    .map(|toolchain| format!("+{toolchain}"));
                let profile = match profile {
                    CommonProfile::Release => "release",
                    CommonProfile::Debug => "dev",
                };
                let workspace_rust_toolchain = rust_toolchain.clone();
                // The crypto and TPM implementation crates have mutually exclusive
                // backend features and are covered by targeted jobs elsewhere.
                flowey::shell_cmd!(
                    rt,
                    "cargo {workspace_rust_toolchain...}
                        hack
                        --workspace
                        --each-feature
                        --locked
                        --keep-going
                        --exclude crypto
                        --exclude tpm_device
                        --exclude tpm_lib
                        --exclude openvmm_hcl_resources
                        --exclude openvmm_resources
                        check
                        --profile {profile}
                    "
                )
                .run()?;

                // Check the resource crates separately so that only their TPM
                // features are excluded.
                flowey::shell_cmd!(
                    rt,
                    "cargo {rust_toolchain...}
                        hack
                        --package openvmm_hcl_resources
                        --package openvmm_resources
                        --each-feature
                        --locked
                        --keep-going
                        --exclude-features tpm
                        check
                        --profile {profile}
                    "
                )
                .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
