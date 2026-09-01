// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::common::CommonArch;
use crate::common::CommonTriple;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsWrite;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub built_artifacts_write: VmmTestsBuiltArtifactsWrite,
        pub target: CommonTriple,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
        ctx.import::<flowey_lib_common::gh_latest_completed_workflow_id::Node>();
        ctx.import::<resolve_artifact::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            built_artifacts_write:
                VmmTestsBuiltArtifactsWrite {
                    flowey_hvlite,
                    nextest_vmm_tests_archive,
                    incubator,
                    prep_steps,
                    test_igvm_agent_rpc_server,
                    openvmm,
                    openvmm_vhost,
                    pipette_windows,
                    pipette_linux_musl,
                    guest_test_uefi,
                    openhcl_standard,
                    openhcl_standard_dev,
                    openhcl_cvm,
                    openhcl_linux_direct,
                    tmks,
                    tmk_vmm,
                    tmk_vmm_linux_musl,
                    vmgstool,
                    vmgstool_dev,
                    tpm_guest_tests_windows,
                    tpm_guest_tests_linux,
                },
            target,
        } = request;

        // TODO: make this configurable with pipeline parameters
        let run_id = ctx.reqv(
            |v| flowey_lib_common::gh_latest_completed_workflow_id::Request {
                repo: "microsoft/openvmm".into(),
                branch: ReadVar::from_static("main".into()),
                pipeline_name: "openvmm-ci.yaml".into(),
                gh_workflow_id: v,
            },
        );

        let arch_tag = match target.common_arch()? {
            CommonArch::X86_64 => "x64",
            CommonArch::Aarch64 => "aarch64",
        };

        let os = target.as_triple().operating_system;
        let os_tag = match os {
            target_lexicon::OperatingSystem::Windows => "windows",
            target_lexicon::OperatingSystem::Linux => "linux",
            _ => anyhow::bail!("unsupported operating system: {:?}", os),
        };

        if flowey_hvlite.is_some() {
            anyhow::bail!("downloading flowey_hvlite is not supported");
        }

        if let Some(nextest_vmm_tests_archive) = nextest_vmm_tests_archive {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-vmm-tests-archive"),
                run_id.clone(),
                nextest_vmm_tests_archive,
            );
        }

        if let Some(incubator) = incubator {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-incubator"),
                run_id.clone(),
                incubator,
            );
        }

        if let Some(prep_steps) = prep_steps {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-prep_steps"),
                run_id.clone(),
                prep_steps,
            );
        }

        if let Some(test_igvm_agent_rpc_server) = test_igvm_agent_rpc_server {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-test_igvm_agent_rpc_server"),
                run_id.clone(),
                test_igvm_agent_rpc_server,
            );
        }

        if let Some(openvmm) = openvmm {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-openvmm"),
                run_id.clone(),
                openvmm,
            );
        }

        if let Some(openvmm_vhost) = openvmm_vhost {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-openvmm_vhost"),
                run_id.clone(),
                openvmm_vhost,
            );
        }

        if let Some(pipette_windows) = pipette_windows {
            download_artifact(
                ctx,
                format!("{arch_tag}-windows-pipette"),
                run_id.clone(),
                pipette_windows,
            );
        }

        if let Some(pipette_linux_musl) = pipette_linux_musl {
            download_artifact(
                ctx,
                format!("{arch_tag}-linux-musl-pipette"),
                run_id.clone(),
                pipette_linux_musl,
            );
        }

        if let Some(guest_test_uefi) = guest_test_uefi {
            download_artifact(
                ctx,
                format!("{arch_tag}-guest_test_uefi"),
                run_id.clone(),
                guest_test_uefi,
            );
        }

        if let Some(openhcl_standard) = openhcl_standard {
            download_artifact(
                ctx,
                format!("{arch_tag}-openhcl-igvm"),
                run_id.clone(),
                openhcl_standard,
            );
        }

        if let Some(openhcl_standard_dev) = openhcl_standard_dev {
            download_artifact(
                ctx,
                format!("{arch_tag}-openhcl-igvm-devkern"),
                run_id.clone(),
                openhcl_standard_dev,
            );
        }

        if let Some(openhcl_cvm) = openhcl_cvm {
            download_artifact(
                ctx,
                format!("{arch_tag}-openhcl-igvm-cvm"),
                run_id.clone(),
                openhcl_cvm,
            );
        }

        if let Some(openhcl_linux_direct) = openhcl_linux_direct {
            download_artifact(
                ctx,
                format!("{arch_tag}-openhcl-igvm-test-linux-direct"),
                run_id.clone(),
                openhcl_linux_direct,
            );
        }

        if let Some(tmks) = tmks {
            download_artifact(ctx, format!("{arch_tag}-tmks"), run_id.clone(), tmks);
        }

        if let Some(tmk_vmm) = tmk_vmm {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-tmk_vmm"),
                run_id.clone(),
                tmk_vmm,
            );
        }

        if let Some(tmk_vmm_linux_musl) = tmk_vmm_linux_musl {
            download_artifact(
                ctx,
                format!("{arch_tag}-linux-musl-tmk_vmm"),
                run_id.clone(),
                tmk_vmm_linux_musl,
            );
        }

        if let Some(vmgstool) = vmgstool {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-vmgstool"),
                run_id.clone(),
                vmgstool,
            );
        }

        if let Some(vmgstool_dev) = vmgstool_dev {
            download_artifact(
                ctx,
                format!("{arch_tag}-{os_tag}-vmgstool-dev"),
                run_id.clone(),
                vmgstool_dev,
            );
        }

        if let Some(tpm_guest_tests_windows) = tpm_guest_tests_windows {
            download_artifact(
                ctx,
                format!("{arch_tag}-windows-tpm_guest_tests"),
                run_id.clone(),
                tpm_guest_tests_windows,
            );
        }

        if let Some(tpm_guest_tests_linux) = tpm_guest_tests_linux {
            download_artifact(
                ctx,
                format!("{arch_tag}-linux-tpm_guest_tests"),
                run_id.clone(),
                tpm_guest_tests_linux,
            );
        }

        Ok(())
    }
}

fn download_artifact<T: Artifact>(
    ctx: &mut NodeCtx<'_>,
    file_name: String,
    run_id: ReadVar<String>,
    output: WriteVar<T>,
) {
    let downloaded_artifact = ctx.reqv(|v| flowey_lib_common::download_gh_artifact::Request {
        repo_owner: "microsoft".into(),
        repo_name: "openvmm".into(),
        file_name,
        path: v,
        run_id,
    });
    ctx.req(resolve_artifact::Request::new(downloaded_artifact, output));
}
