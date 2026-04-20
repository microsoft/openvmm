// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compares the size of the OpenHCL binary and kernel binaries in the current
//! PR with the sizes from the last successful merge to main.

use crate::_jobs::build_and_publish_openvmm_hcl_baseline::KernelCheck;
use crate::build_openhcl_igvm_from_recipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl;
use crate::build_openvmm_hcl::OpenvmmHclBuildParams;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile::OpenvmmHclShip;
use crate::common::CommonArch;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::download_gh_artifact;
use flowey_lib_common::gh_workflow_id;
use flowey_lib_common::git_merge_commit;

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub kernel_checks: Vec<KernelCheck>,
        pub done: WriteVar<SideEffect>,
        pub pipeline_name: String,
        pub job_name: String,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
        ctx.import::<download_gh_artifact::Node>();
        ctx.import::<git_merge_commit::Node>();
        ctx.import::<gh_workflow_id::Node>();
        ctx.import::<build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<build_openvmm_hcl::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            kernel_checks,
            done,
            pipeline_name,
            job_name,
        } = request;

        let arch = target.common_arch().unwrap();

        let xtask_target = CommonTriple::Common {
            arch: ctx.arch().try_into()?,
            platform: ctx.platform().try_into()?,
        };

        let xtask = ctx.reqv(|v| crate::build_xtask::Request {
            target: xtask_target,
            xtask: v,
        });
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let recipe = match arch {
            CommonArch::X86_64 => OpenhclIgvmRecipe::X64,
            CommonArch::Aarch64 => OpenhclIgvmRecipe::Aarch64,
        }
        .recipe_details(true);

        let built_openvmm_hcl = ctx.reqv(|v| build_openvmm_hcl::Request {
            build_params: OpenvmmHclBuildParams {
                target: target.clone(),
                profile: OpenvmmHclShip,
                features: recipe.openvmm_hcl_features,
                no_split_dbg_info: false,
                max_trace_level: recipe.max_trace_level,
            },
            openvmm_hcl_output: v,
        });

        let kernel_arch = arch;

        let current_kernels: Vec<_> = kernel_checks
            .iter()
            .map(|kc| {
                let kernel =
                    ctx.reqv(
                        |v| crate::resolve_openhcl_kernel_package::Request::GetKernel {
                            kind: kc.kind,
                            arch: kernel_arch,
                            kernel: v,
                        },
                    );
                (kc.label.clone(), kernel)
            })
            .collect();

        let file_name = match arch {
            CommonArch::X86_64 => "x64-openhcl-baseline",
            CommonArch::Aarch64 => "aarch64-openhcl-baseline",
        };

        let merge_commit = ctx.reqv(|v| git_merge_commit::Request {
            repo_path: openvmm_repo_path.clone(),
            merge_commit: v,
            base_branch: "main".into(),
        });

        let merge_run = ctx.reqv(|v| {
            gh_workflow_id::Request::WithStatusAndJob(gh_workflow_id::QueryWithStatusAndJob {
                params: gh_workflow_id::WorkflowQueryParams {
                    github_commit_hash: merge_commit,
                    repo_path: openvmm_repo_path.clone(),
                    pipeline_name,
                    gh_workflow: v,
                },
                gh_run_status: gh_workflow_id::GhRunStatus::Completed,
                gh_run_job_name: job_name,
            })
        });

        let run_id = merge_run.map(ctx, |r| r.id);
        let merge_head_artifact = ctx.reqv(|old_openhcl| download_gh_artifact::Request {
            repo_owner: "microsoft".into(),
            repo_name: "openvmm".into(),
            file_name: file_name.into(),
            path: old_openhcl,
            run_id,
        });

        // Publish the built binary as an artifact for offline analysis.
        let publish_artifact = if ctx.backend() == FlowBackend::Github {
            let dir = ctx.emit_rust_stepv("collect openvmm_hcl files for analysis", |ctx| {
                let built_openvmm_hcl = built_openvmm_hcl.clone().claim(ctx);
                move |rt| {
                    let built_openvmm_hcl = rt.read(built_openvmm_hcl);
                    let path = Path::new("artifact");
                    fs_err::create_dir_all(path)?;
                    fs_err::copy(built_openvmm_hcl.bin, path.join("openvmm_hcl"))?;
                    if let Some(dbg) = built_openvmm_hcl.dbg {
                        fs_err::copy(dbg, path.join("openvmm_hcl.dbg"))?;
                    }
                    Ok(path
                        .absolute()?
                        .into_os_string()
                        .into_string()
                        .ok()
                        .unwrap())
                }
            });
            let name = format!(
                "{}_openvmm_hcl_for_size_analysis",
                target.common_arch().unwrap().as_arch()
            );
            Some(
                ctx.emit_gh_step(
                    "publish openvmm_hcl for analysis",
                    "actions/upload-artifact@v7",
                )
                .with("name", name)
                .with("path", dir)
                .finish(ctx),
            )
        } else {
            None
        };

        let comparison = ctx.emit_rust_step("binary size comparison", |ctx| {
            let _publish_artifact = publish_artifact.claim(ctx);
            let xtask = xtask.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let old_openhcl = merge_head_artifact.claim(ctx);
            let new_openhcl = built_openvmm_hcl.claim(ctx);
            let merge_run = merge_run.claim(ctx);
            let current_kernels: Vec<_> = current_kernels
                .into_iter()
                .map(|(label, k)| (label, k.claim(ctx)))
                .collect();

            move |rt| {
                let xtask = match rt.read(xtask) {
                    crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                    crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                };

                let old_openhcl = rt.read(old_openhcl);
                let new_openhcl = rt.read(new_openhcl);
                let merge_run = rt.read(merge_run);

                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(&path);

                println!(
                    "comparing HEAD to merge commit {} and workflow {}",
                    merge_run.commit, merge_run.id
                );

                // Compare usermode binary
                let old_path = old_openhcl.join(file_name).join("openhcl");
                let new_path = &new_openhcl.bin;
                println!("== openvmm_hcl usermode binary ==");
                flowey::shell_cmd!(
                    rt,
                    "{xtask} verify-size --original {old_path} --new {new_path}"
                )
                .run()?;

                // Compare kernel binaries
                for (label, kernel_var) in current_kernels {
                    anyhow::ensure!(
                        !label.is_empty()
                            && !label.contains('/')
                            && !label.contains('\\')
                            && !label.starts_with('.'),
                        "invalid kernel label: {label}"
                    );
                    let new_kernel = rt.read(kernel_var);
                    let old_kernel = old_openhcl.join(file_name).join(&label);
                    println!("== kernel: {label} ==");
                    flowey::shell_cmd!(
                        rt,
                        "{xtask} verify-size --original {old_kernel} --new {new_kernel}"
                    )
                    .run()?;
                }

                Ok(())
            }
        });

        ctx.emit_side_effect_step(vec![comparison], [done]);

        Ok(())
    }
}
