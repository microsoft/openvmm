// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that supports the `cargo xflowey run-igvm` CLI

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm;
use crate::build_openvmm::OpenvmmBuildParams;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
        pub artifact_dir: ReadVar<PathBuf>,
        pub base_recipe: OpenhclIgvmRecipe,
        pub release: bool,
        pub customizations: crate::_jobs::local_build_igvm::Customizations,
        pub openvmm_args: Vec<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::_jobs::local_build_igvm::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<build_openvmm::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Self::Request {
            done,
            artifact_dir,
            release,
            base_recipe,
            customizations,
            openvmm_args,
        } = request;

        let mut side_effects = Vec::new();

        let (read_bin_path, write_bin_path) = ctx.new_var();

        let built_igvm = ctx.reqv(|v| crate::_jobs::local_build_igvm::Params {
            customizations,
            release,
            artifact_dir,
            base_recipe,
            bin_path: Some(write_bin_path),
            done: v,
        });

        side_effects.push(built_igvm);

        let built_openvmm = ctx.reqv(|v| build_openvmm::Request {
            params: OpenvmmBuildParams {
                profile: CommonProfile::Debug,
                target: CommonTriple::X86_64_WINDOWS_MSVC,
                features: Default::default(),
            },
            openvmm: v,
        });

        side_effects.push(ctx.emit_rust_step("run openvmm", |ctx| {
            let built_openvmm = built_openvmm.claim(ctx);
            let built_igvm = read_bin_path.claim(ctx);
            |rt| {
                let built_openvmm = rt.read(built_openvmm);
                let built_igvm = rt.read(built_igvm);
                if let build_openvmm::OpenvmmOutput::WindowsBin { exe, pdb: _ } = built_openvmm {
                    let sh = xshell::Shell::new()?;
                    xshell::cmd!(sh, "{exe} --igvm {built_igvm} {openvmm_args...}").run()?;
                }

                Ok(())
            }
        }));

        ctx.emit_side_effect_step(side_effects, vec![done]);

        Ok(())
    }
}
