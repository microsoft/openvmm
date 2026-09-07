// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Set up, run, and publish a standalone VMM.Perf job.

use crate::build_openvmm::OpenvmmOutput;
use crate::build_vmm_perf::VmmPerfOutput;
use crate::common::CommonArch;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDepsLinux;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDepsWindows;
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
        /// Local runtime archive override. CI always uses the pinned download.
        pub runtime_archive: Option<ReadVar<PathBuf>>,
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
        ctx.import::<crate::install_vmm_tests_external_deps::Node>();
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
            runtime_archive,
            root_dir,
            hugetlb_2mb_overcommit_pages,
            done,
        } = request;

        if root_dir.is_some() && !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("custom VMM.Perf root directories are local-only");
        }
        if runtime_archive.is_some() && !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("custom VMM.Perf runtime archives are local-only");
        }

        let external_deps = match ctx.platform() {
            FlowPlatform::Windows => VmmTestsExternalDeps::Windows(VmmTestsExternalDepsWindows {
                hyperv: true,
                whp: true,
                hardware_isolation: false,
            }),
            FlowPlatform::Linux(_) => VmmTestsExternalDeps::Linux(VmmTestsExternalDepsLinux {
                hugetlb_2mb_overcommit_pages,
                prepare_vhost_vsock: false,
            }),
            _ => anyhow::bail!("VMM.Perf is unsupported on this platform"),
        };
        ctx.config(crate::install_vmm_tests_external_deps::Config {
            selections: Some(external_deps),
            auto_install: None,
        });
        let pre_run_deps = vec![ctx.reqv(crate::install_vmm_tests_external_deps::Request::Install)];

        let firmware = ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
            arch: CommonArch::X86_64,
            msvm_fd: v,
        });
        let runtime_archive = match runtime_archive {
            Some(runtime_archive) => runtime_archive,
            None => ctx.reqv(|v| crate::download_vmm_perf_runtime::Request::Get {
                arch: CommonArch::X86_64,
                runtime_archive: v,
            }),
        };
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
            pre_run_deps,
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
