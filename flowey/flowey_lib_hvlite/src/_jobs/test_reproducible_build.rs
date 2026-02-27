// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Job node that runs `cargo xflowey build-reproducible` in CI and publishes
//! the resulting IGVM artifact.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Recipe name to pass to `cargo xflowey build-reproducible` (e.g. "x64-cvm").
        pub recipe: String,
        /// Directory to publish the local build-reproducible igvm output to.
        pub artifact_dir_local_igvm: ReadVar<PathBuf>,
        /// Directory to publish the local build-reproducible igvm extras output to.
        pub artifact_dir_local_igvm_extras: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_nix::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            recipe,
            artifact_dir_local_igvm,
            artifact_dir_local_igvm_extras,
            done,
        } = request;

        let hvlite_repo = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let rust_install = ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled);
        let nix_install = ctx.reqv(flowey_lib_common::install_nix::Request::EnsureInstalled);

        let step = ctx.emit_rust_step("run cargo xflowey build-reproducible", |ctx| {
            rust_install.claim(ctx);
            nix_install.claim(ctx);
            let hvlite_repo = hvlite_repo.claim(ctx);
            let artifact_dir_local_igvm = artifact_dir_local_igvm.claim(ctx);
            let artifact_dir_local_igvm_extras = artifact_dir_local_igvm_extras.claim(ctx);
            let recipe = recipe.clone();
            move |rt| {
                let hvlite_repo = rt.read(hvlite_repo);
                let publish_dir = rt.read(artifact_dir_local_igvm);
                let publish_dir_extras = rt.read(artifact_dir_local_igvm_extras);

                rt.sh.change_dir(&hvlite_repo);

                let shell_nix = hvlite_repo.join("shell.nix");
                log::info!("compiling flowey_hvlite inside nix-shell");
                flowey::shell_cmd!(rt, "nix-shell {shell_nix} --pure --run")
                    .arg("cargo build -p flowey_hvlite")
                    .run()?;

                let flowey_bin = hvlite_repo.join("target/debug/flowey_hvlite");
                log::info!("running {flowey_bin:?} pipeline run build-reproducible {recipe}");
                flowey::shell_cmd!(
                    rt,
                    "{flowey_bin} pipeline run build-reproducible {recipe} --release"
                )
                .env(
                    "I_HAVE_A_GOOD_REASON_TO_RUN_BUILD_REPRODUCIBLE_IN_CI",
                    "true",
                )
                .run()?;

                let local_igvm_dir = hvlite_repo.join("flowey-out/artifacts/x64-cvm-openhcl-igvm");
                let local_igvm_extras_dir =
                    hvlite_repo.join("flowey-out/artifacts/x64-cvm-openhcl-igvm-extras");

                log::info!("publishing local build output");
                flowey::util::copy_dir_all(&local_igvm_dir, &publish_dir)?;
                flowey::util::copy_dir_all(&local_igvm_extras_dir, &publish_dir_extras)?;

                Ok(())
            }
        });

        ctx.emit_side_effect_step([step], [done]);

        Ok(())
    }
}
