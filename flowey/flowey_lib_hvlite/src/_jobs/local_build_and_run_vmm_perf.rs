// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run VMM.Perf locally.

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::run_vmm_perf::VmmPerfProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub root_dir: PathBuf,
        pub profiles: Vec<VmmPerfProfile>,
        pub vm_sizes_json: Option<String>,
        pub parameters_json: Option<String>,
        pub build_only: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_vmm_perf::Node>();
        ctx.import::<crate::_jobs::setup_and_run_vmm_perf::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            root_dir,
            profiles,
            vm_sizes_json,
            parameters_json,
            build_only,
            done,
        } = request;

        let openvmm = ctx.reqv(|v| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                target: target.clone(),
                profile,
                features: [crate::build_openvmm::OpenvmmFeature::Tpm].into(),
            },
            openvmm: v,
        });
        let runner = ctx.reqv(|v| crate::build_vmm_perf::Request {
            target,
            profile,
            vmm_perf: v,
        });

        if build_only {
            ctx.emit_side_effect_step(
                [openvmm.into_side_effect(), runner.into_side_effect()],
                [done],
            );
        } else {
            ctx.req(crate::_jobs::setup_and_run_vmm_perf::Params {
                label: "vmm-perf".into(),
                runner,
                openvmm,
                profiles,
                vm_sizes_json,
                parameters_json,
                root_dir: Some(ReadVar::from_static(root_dir)),
                hugetlb_2mb_overcommit_pages: None,
                done,
            });
        }

        Ok(())
    }
}
