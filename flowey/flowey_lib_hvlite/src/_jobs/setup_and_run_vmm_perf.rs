// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Set up, run, and publish a standalone VMM.Perf job.

use crate::build_openvmm::OpenvmmOutput;
use crate::build_vmm_perf::VmmPerfOutput;
use crate::common::CommonArch;
use crate::run_vmm_perf::VmmPerfProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Params {
        pub label: String,
        pub runner: ReadVar<VmmPerfOutput>,
        pub openvmm: ReadVar<OpenvmmOutput>,
        pub profiles: Vec<VmmPerfProfile>,
        pub vm_sizes_json: Option<String>,
        pub parameters_json: Option<String>,
        /// Local-only root directory. CI uses a job-local staging directory.
        pub root_dir: Option<ReadVar<PathBuf>>,
        pub hugetlb_2mb_overcommit_pages: Option<u64>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::download_vmm_perf_runtime::Node>();
        ctx.import::<crate::run_vmm_perf::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            label,
            runner,
            openvmm,
            profiles,
            vm_sizes_json,
            parameters_json,
            root_dir,
            hugetlb_2mb_overcommit_pages,
            done,
        } = request;

        if root_dir.is_some() && !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("custom VMM.Perf root directories are local-only");
        }

        let firmware = ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
            arch: CommonArch::X86_64,
            msvm_fd: v,
        });
        let runtime_archive = ctx.reqv(|v| crate::download_vmm_perf_runtime::Request::Get {
            arch: CommonArch::X86_64,
            runtime_archive: v,
        });
        let job_root = match ctx.backend() {
            FlowBackend::Local => root_dir
                .ok_or_else(|| anyhow::anyhow!("local VMM.Perf runs require a root directory"))?,
            FlowBackend::Ado => ctx
                .get_ado_variable(AdoRuntimeVar::PIPELINE_WORKSPACE)
                .map(ctx, |root| PathBuf::from(root).join("vp")),
            FlowBackend::Github => ctx
                .get_gh_context_var()
                .global()
                .runner_temp()
                .map(ctx, |root| PathBuf::from(root).join("vp")),
        };
        let output_dir = job_root.clone().map(ctx, |root| root.join("results"));
        let temp_dir = Some(job_root.map(ctx, |root| root.join("t")));

        let result = ctx.reqv(|v| crate::run_vmm_perf::Request {
            runner,
            openvmm,
            firmware,
            runtime_archive,
            output_dir,
            temp_dir,
            profiles,
            vm_sizes_json,
            parameters_json,
            hugetlb_2mb_overcommit_pages,
            output: v,
        });

        let publish_done = if matches!(ctx.backend(), FlowBackend::Local) {
            result.clone().into_side_effect()
        } else {
            let results_dir = result.clone().map(ctx, |result| result.results_dir);
            let test_results = result.clone().map(ctx, |result| {
                flowey_lib_common::run_cargo_nextest_run::TestResults {
                    all_tests_passed: result.success,
                    junit_xml: None,
                }
            });
            ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                test_results,
                test_label: label,
                attachments: BTreeMap::from([("results".into(), (results_dir, false))]),
                output_dir: None,
                upload_logs_on_success: true,
                done: v,
            })
        };

        ctx.emit_rust_step("report VMM.Perf result", |ctx| {
            let result = result.claim(ctx);
            publish_done.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let result = rt.read(result);
                anyhow::ensure!(
                    result.success,
                    "VMM.Perf failed with exit code {}",
                    result
                        .exit_code
                        .map_or_else(|| "unknown".into(), |code| code.to_string())
                );
                Ok(())
            }
        });

        Ok(())
    }
}
