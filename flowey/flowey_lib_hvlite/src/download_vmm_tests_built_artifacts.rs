// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsWrite;
use flowey::node::prelude::*;
use flowey_lib_common::gh_workflow_id;

flowey_request! {
    pub struct Request {
        pub built_artifacts_write: VmmTestsBuiltArtifactsWrite,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
        ctx.import::<gh_workflow_id::Node>();
        ctx.import::<resolve_artifact::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            built_artifacts_write:
                VmmTestsBuiltArtifactsWrite {
                    flowey_hvlite_windows_x64,
                    flowey_hvlite_windows_aarch64,
                    flowey_hvlite_linux_x64,
                    nextest_vmm_tests_archive_windows_x64,
                    nextest_vmm_tests_archive_windows_aarch64,
                    nextest_vmm_tests_archive_linux_x64,
                    nextest_vmm_tests_archive_linux_musl_x64,
                    nextest_vmm_tests_archive_linux_musl_aarch64,
                    incubator_linux_x64,
                    prep_steps_windows_x64,
                    prep_steps_linux_musl_x64,
                    test_igvm_agent_rpc_server_windows_x64,
                    openvmm_windows_x64,
                    openvmm_windows_aarch64,
                    openvmm_linux_x64,
                    openvmm_linux_aarch64,
                    openvmm_linux_musl_x64,
                    openvmm_linux_musl_aarch64,
                    openvmm_vhost_linux_x64,
                    openvmm_vhost_linux_aarch64,
                    openvmm_vhost_linux_musl_x64,
                    openvmm_vhost_linux_musl_aarch64,
                    pipette_windows_x64,
                    pipette_windows_aarch64,
                    pipette_linux_musl_x64,
                    pipette_linux_musl_aarch64,
                    guest_test_uefi_x64,
                    guest_test_uefi_aarch64,
                    openhcl_standard_x64,
                    openhcl_standard_aarch64,
                    openhcl_standard_dev_x64,
                    openhcl_standard_dev_aarch64,
                    openhcl_cvm_x64,
                    openhcl_linux_direct_x64,
                    tmks_x64,
                    tmks_aarch64,
                    tmk_vmm_windows_x64,
                    tmk_vmm_windows_aarch64,
                    tmk_vmm_linux_musl_x64,
                    tmk_vmm_linux_musl_aarch64,
                    vmgstool_windows_x64,
                    vmgstool_windows_aarch64,
                    vmgstool_linux_x64,
                    vmgstool_dev_windows_x64,
                    vmgstool_dev_windows_aarch64,
                    vmgstool_dev_linux_x64,
                    tpm_guest_tests_windows_x64,
                    tpm_guest_tests_linux_x64,
                },
        } = request;

        let run = ctx.reqv(|v| gh_workflow_id::Request {
            repo_owner: "microsoft".into(),
            repo_name: "openvmm".into(),
            commit_or_branch: gh_workflow_id::GitCommitOrBranch::Branch(ReadVar::from_static(
                "main".into(),
            )),
            pipeline_name: "openvmm-ci.yaml".into(),
            require_run_status: Some(gh_workflow_id::GhRunStatus::Success),
            require_successful_job_with_name: None,
            gh_workflow: v,
        });
        let run_id = run.map(ctx, |r| r.id);

        macro_rules! download {
            ($output:expr, $file_name:literal) => {
                if let Some(output) = $output {
                    download_artifact(ctx, $file_name.into(), run_id.clone(), output);
                }
            };
        }

        if flowey_hvlite_windows_x64.is_some()
            || flowey_hvlite_windows_aarch64.is_some()
            || flowey_hvlite_linux_x64.is_some()
        {
            anyhow::bail!("downloading flowey_hvlite is not supported");
        }

        download!(
            nextest_vmm_tests_archive_windows_x64,
            "x64-windows-vmm-tests-archive"
        );
        download!(
            nextest_vmm_tests_archive_windows_aarch64,
            "aarch64-windows-vmm-tests-archive"
        );
        download!(
            nextest_vmm_tests_archive_linux_x64,
            "x64-linux-vmm-tests-archive"
        );
        download!(
            nextest_vmm_tests_archive_linux_musl_x64,
            "x64-linux-musl-vmm-tests-archive"
        );
        download!(
            nextest_vmm_tests_archive_linux_musl_aarch64,
            "aarch64-linux-musl-vmm-tests-archive"
        );

        download!(incubator_linux_x64, "x64-linux-incubator");
        download!(prep_steps_windows_x64, "x64-windows-prep_steps");
        download!(prep_steps_linux_musl_x64, "x64-linux-musl-prep_steps");
        download!(
            test_igvm_agent_rpc_server_windows_x64,
            "x64-windows-test_igvm_agent_rpc_server"
        );

        download!(openvmm_windows_x64, "x64-windows-openvmm");
        download!(openvmm_windows_aarch64, "aarch64-windows-openvmm");
        download!(openvmm_linux_x64, "x64-linux-openvmm");
        download!(openvmm_linux_aarch64, "aarch64-linux-openvmm");
        download!(openvmm_linux_musl_x64, "x64-linux-musl-openvmm");
        download!(openvmm_linux_musl_aarch64, "aarch64-linux-musl-openvmm");

        download!(openvmm_vhost_linux_x64, "x64-linux-openvmm_vhost");
        download!(openvmm_vhost_linux_aarch64, "aarch64-linux-openvmm_vhost");
        download!(openvmm_vhost_linux_musl_x64, "x64-linux-musl-openvmm_vhost");
        download!(
            openvmm_vhost_linux_musl_aarch64,
            "aarch64-linux-musl-openvmm_vhost"
        );

        download!(pipette_windows_x64, "x64-windows-pipette");
        download!(pipette_windows_aarch64, "aarch64-windows-pipette");
        download!(pipette_linux_musl_x64, "x64-linux-musl-pipette");
        download!(pipette_linux_musl_aarch64, "aarch64-linux-musl-pipette");

        download!(guest_test_uefi_x64, "x64-guest_test_uefi");
        download!(guest_test_uefi_aarch64, "aarch64-guest_test_uefi");

        download!(openhcl_standard_x64, "x64-openhcl-igvm");
        download!(openhcl_standard_aarch64, "aarch64-openhcl-igvm");
        download!(openhcl_standard_dev_x64, "x64-openhcl-igvm-devkern");
        download!(openhcl_standard_dev_aarch64, "aarch64-openhcl-igvm-devkern");
        download!(openhcl_cvm_x64, "x64-openhcl-igvm-cvm");
        download!(
            openhcl_linux_direct_x64,
            "x64-openhcl-igvm-test-linux-direct"
        );

        download!(tmks_x64, "x64-tmks");
        download!(tmks_aarch64, "aarch64-tmks");

        download!(tmk_vmm_windows_x64, "x64-windows-tmk_vmm");
        download!(tmk_vmm_windows_aarch64, "aarch64-windows-tmk_vmm");
        download!(tmk_vmm_linux_musl_x64, "x64-linux-musl-tmk_vmm");
        download!(tmk_vmm_linux_musl_aarch64, "aarch64-linux-musl-tmk_vmm");

        download!(vmgstool_windows_x64, "x64-windows-vmgstool");
        download!(vmgstool_windows_aarch64, "aarch64-windows-vmgstool");
        download!(vmgstool_linux_x64, "x64-linux-vmgstool");

        download!(vmgstool_dev_windows_x64, "x64-windows-vmgstool-dev");
        download!(vmgstool_dev_windows_aarch64, "aarch64-windows-vmgstool-dev");
        download!(vmgstool_dev_linux_x64, "x64-linux-vmgstool-dev");

        download!(tpm_guest_tests_windows_x64, "x64-windows-tpm_guest_tests");
        download!(tpm_guest_tests_linux_x64, "x64-linux-tpm_guest_tests");

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
