// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an OpenHCL binary and kernel binaries for size
//! comparison with PRs.

use crate::artifact_openvmm_hcl_sizecheck;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl;
use crate::build_openvmm_hcl::OpenvmmHclBuildParams;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageArch;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

/// A kernel to include in the baseline artifact.
#[derive(Clone, Serialize, Deserialize)]
pub struct KernelCheck {
    pub kind: OpenhclKernelPackageKind,
    pub label: String,
}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub kernel_checks: Vec<KernelCheck>,
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<build_openvmm_hcl::Node>();
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            kernel_checks,
            done,
            artifact_dir,
        } = request;

        let arch = target.common_arch().unwrap();

        let recipe = match arch {
            CommonArch::X86_64 => OpenhclIgvmRecipe::X64,
            CommonArch::Aarch64 => OpenhclIgvmRecipe::Aarch64,
        }
        .recipe_details(true);

        let baseline_hcl_build = ctx.reqv(|v| build_openvmm_hcl::Request {
            build_params: OpenvmmHclBuildParams {
                target,
                profile: OpenvmmHclBuildProfile::OpenvmmHclShip,
                features: recipe.openvmm_hcl_features,
                no_split_dbg_info: false,
                max_trace_level: recipe.max_trace_level,
            },
            openvmm_hcl_output: v,
        });

        let kernel_arch = match arch {
            CommonArch::X86_64 => OpenhclKernelPackageArch::X86_64,
            CommonArch::Aarch64 => OpenhclKernelPackageArch::Aarch64,
        };

        let kernel_baselines: Vec<_> = kernel_checks
            .into_iter()
            .map(|kc| {
                let kernel =
                    ctx.reqv(
                        |v| crate::resolve_openhcl_kernel_package::Request::GetKernel {
                            kind: kc.kind,
                            arch: kernel_arch,
                            kernel: v,
                        },
                    );
                artifact_openvmm_hcl_sizecheck::publish::KernelBaseline {
                    label: kc.label,
                    kernel,
                }
            })
            .collect();

        ctx.req(artifact_openvmm_hcl_sizecheck::publish::Request {
            openvmm_openhcl: baseline_hcl_build,
            kernel_baselines,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
