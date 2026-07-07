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
use crate::_jobs::build_and_publish_openvmm_hcl_baseline::OpenvmmHclBaselineOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmExtrasOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::common::CommonArch;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub arch: CommonArch,
        pub kernel_kind: OpenhclKernelPackageKind,
        pub igvm_files: Vec<(OpenhclIgvmBuildParams, WriteVar<OpenhclIgvmOutput>, WriteVar<OpenhclIgvmExtrasOutput>)>,
        pub artifact_openhcl_verify_size_baseline: Option<WriteVar<OpenvmmHclBaselineOutput>>,
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
            artifact_openhcl_verify_size_baseline,
        } = request;

        ctx.req(crate::_jobs::cfg_nix::Params { arch, kernel_kind });

        ctx.req(
            crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::Params {
                igvm_files,
                artifact_openhcl_verify_size_baseline,
            },
        );

        Ok(())
    }
}
