// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub x64_direct_bin: WriteVar<PathBuf>,
        pub x64_bin: WriteVar<PathBuf>,
        pub aarch64_bin: WriteVar<PathBuf>
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::git_latest_commit::Node>();
        ctx.import::<flowey_lib_common::gh_workflow_id::Node>();
        ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            x64_direct_bin,
            x64_bin,
            aarch64_bin,
        } = request;

        // Download 2411 release IGVM files for running servicing tests
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let latest_commit = ctx.reqv(|v| flowey_lib_common::git_latest_commit::Request {
            repo_path: openvmm_repo_path.clone(),
            branch: "release/2411".into(),
            commit: v,
        });

        let run = ctx.reqv(|v| flowey_lib_common::gh_workflow_id::Request {
            github_commit_hash: latest_commit,
            repo_path: openvmm_repo_path,
            pipeline_name: "[flowey] OpenVMM CI".into(),
            gh_workflow: v,
        });

        let run_id = run.map(ctx, |r| r.id);

        let mut downloaded_aarch64 = None;
        let mut downloaded_x64 = None;

        let arches = vec![CommonArch::X86_64, CommonArch::Aarch64];
        for arch in arches.clone() {
            let arch_str = match arch {
                CommonArch::X86_64 => "x64",
                CommonArch::Aarch64 => "aarch64",
            };

            let downloaded = ctx.reqv(|v| flowey_lib_common::download_gh_artifact::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm".into(),
                file_name: format!("{arch_str}-openhcl-igvm").into(),
                path: v,
                run_id: run_id.clone(),
            });

            if arch == CommonArch::X86_64 {
                downloaded_x64 = Some(downloaded);
            } else {
                downloaded_aarch64 = Some(downloaded);
            }
        }

        ctx.emit_rust_step("write to directory variables", |ctx| {
            let downloaded_x64 = downloaded_x64.unwrap().claim(ctx);
            let downloaded_aarch64 = downloaded_aarch64.unwrap().claim(ctx);

            let write_downloaded_x64 = x64_bin.claim(ctx);
            let write_downloaded_x64_direct = x64_direct_bin.claim(ctx);
            let write_downloaded_aarch64 = aarch64_bin.claim(ctx);

            |rt| {
                let downloaded_x64 = rt.read(downloaded_x64);
                let downloaded_aarch64 = rt.read(downloaded_aarch64);

                rt.write(write_downloaded_x64, &downloaded_x64.join("openhcl.bin"));
                rt.write(
                    write_downloaded_x64_direct,
                    &downloaded_x64.join("openhcl-direct.bin"),
                );
                rt.write(
                    write_downloaded_aarch64,
                    &downloaded_aarch64.join("openhcl-aarch64.bin"),
                );

                Ok(())
            }
        });

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct Release2411Output {
    pub x64_direct_bin: PathBuf,
    pub x64_bin: PathBuf,
    pub aarch64_bin: PathBuf,
}

pub mod resolve {
    use super::Release2411Output;
    use crate::run_cargo_build::common::CommonArch;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub release_output: WriteVar<Release2411Output>
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<crate::git_checkout_openvmm_repo::Node>();
            ctx.import::<flowey_lib_common::git_latest_commit::Node>();
            ctx.import::<flowey_lib_common::gh_workflow_id::Node>();
            ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request { release_output } = request;

            // Download 2411 release IGVM files for running servicing tests
            let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
            let latest_commit = ctx.reqv(|v| flowey_lib_common::git_latest_commit::Request {
                repo_path: openvmm_repo_path.clone(),
                branch: "release/2411".into(),
                commit: v,
            });

            let run = ctx.reqv(|v| flowey_lib_common::gh_workflow_id::Request {
                github_commit_hash: latest_commit,
                repo_path: openvmm_repo_path,
                pipeline_name: "[flowey] OpenVMM CI".into(),
                gh_workflow: v,
            });

            let run_id = run.map(ctx, |r| r.id);

            let mut downloaded_aarch64 = None;
            let mut downloaded_x64 = None;

            let arches = vec![CommonArch::X86_64, CommonArch::Aarch64];
            for arch in arches.clone() {
                let arch_str = match arch {
                    CommonArch::X86_64 => "x64",
                    CommonArch::Aarch64 => "aarch64",
                };

                let downloaded = ctx.reqv(|v| flowey_lib_common::download_gh_artifact::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "openvmm".into(),
                    file_name: format!("{arch_str}-openhcl-igvm").into(),
                    path: v,
                    run_id: run_id.clone(),
                });

                if arch == CommonArch::X86_64 {
                    downloaded_x64 = Some(downloaded);
                } else {
                    downloaded_aarch64 = Some(downloaded);
                }
            }

            ctx.emit_rust_step("write to directory variables", |ctx| {
                let downloaded_x64 = downloaded_x64.unwrap().claim(ctx);
                let downloaded_aarch64 = downloaded_aarch64.unwrap().claim(ctx);

                let write_release_output = release_output.claim(ctx);

                |rt| {
                    let downloaded_x64 = rt.read(downloaded_x64);
                    let downloaded_aarch64 = rt.read(downloaded_aarch64);

                    rt.write(
                        write_release_output,
                        &Release2411Output {
                            x64_direct_bin: downloaded_x64.join("openhcl-direct.bin"),
                            x64_bin: downloaded_x64.join("openhcl.bin"),
                            aarch64_bin: downloaded_aarch64.join("openhcl-aarch64.bin"),
                        },
                    );

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
