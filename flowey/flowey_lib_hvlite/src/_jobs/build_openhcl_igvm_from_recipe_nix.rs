// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Composite job node that wires together Nix configuration and the OpenHCL
//! IGVM build-and-publish step.
//!
//! This node exists so that both the local `build-reproducible` pipeline and
//! future CI jobs can share the same wiring without divergence.
//!
//! Note: `cfg_hvlite_reposource` is intentionally excluded, since pipelines
//! like `checkin_gates` inject it across all jobs via `inject_all_jobs_with`.

use crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::OpenhclIgvmBuildParams;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub arch: CommonArch,
        pub kernel_kind: OpenhclKernelPackageKind,
        pub igvm_files: Vec<OpenhclIgvmBuildParams>,
        pub artifact_dir_openhcl_igvm: ReadVar<PathBuf>,
        pub artifact_dir_openhcl_igvm_extras: ReadVar<PathBuf>,
        pub artifact_openhcl_verify_size_baseline: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::_jobs::cfg_nix::Node>();
        ctx.import::<crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            kernel_kind,
            igvm_files,
            artifact_dir_openhcl_igvm,
            artifact_dir_openhcl_igvm_extras,
            artifact_openhcl_verify_size_baseline,
            done,
        } = request;

        ctx.req(crate::_jobs::cfg_nix::Params { arch, kernel_kind });

        ctx.req(
            crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::Params {
                igvm_files,
                artifact_dir_openhcl_igvm,
                artifact_dir_openhcl_igvm_extras,
                artifact_openhcl_verify_size_baseline,
                done,
            },
        );

        Ok(())
    }
}
