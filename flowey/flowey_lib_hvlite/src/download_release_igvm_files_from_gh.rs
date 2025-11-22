// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct ReleaseOutput {
    // Individual artifact paths (may reside in different directories)
    pub openhcl_direct: Option<PathBuf>,
    pub openhcl: Option<PathBuf>,
    pub openhcl_aarch64: Option<PathBuf>,
}

impl Artifact for ReleaseOutput {}

#[derive(Serialize, Deserialize, Clone, Debug, Copy)]
pub enum OpenhclReleaseVersion {
    Release2411,
    Release2505,
    Release2511,
}

impl OpenhclReleaseVersion {
    pub fn branch_name(&self) -> String {
        match self {
            OpenhclReleaseVersion::Release2411 => "release/2411".to_string(),
            OpenhclReleaseVersion::Release2505 => "release/2505".to_string(),
            OpenhclReleaseVersion::Release2511 => "release/1.7.2511".to_string(),
        }
    }

    pub const ALL: [OpenhclReleaseVersion; 3] = [
        OpenhclReleaseVersion::Release2411,
        OpenhclReleaseVersion::Release2505,
        OpenhclReleaseVersion::Release2511,
    ];

    pub fn latest() -> Self {
        *Self::ALL.last().unwrap()
    }
}

impl std::fmt::Display for OpenhclReleaseVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            OpenhclReleaseVersion::Release2411 => "release-2411",
            OpenhclReleaseVersion::Release2505 => "release-2505",
            OpenhclReleaseVersion::Release2511 => "release-2511",
        };
        f.write_str(s)
    }
}

pub mod resolve {
    use super::OpenhclReleaseVersion;
    use super::ReleaseOutput;
    use crate::run_cargo_build::common::CommonArch;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub release_igvm_files: WriteVar<ReleaseOutput>,
            pub release_version: OpenhclReleaseVersion,
            pub arch: CommonArch,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
            ctx.import::<flowey_lib_common::gh_latest_completed_workflow_id::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let branch_name: ReadVar<String> =
                ReadVar::from_static(request.release_version.branch_name());

            let run_id =
                ctx.reqv(
                    |v| flowey_lib_common::gh_latest_completed_workflow_id::Request {
                        repo: "microsoft/openvmm".into(),
                        branch: branch_name.clone(),
                        pipeline_name: "openvmm-ci.yaml".into(),
                        gh_workflow_id: v,
                    },
                );
            let output = request.release_igvm_files;

            let arch_str = match request.arch {
                CommonArch::X86_64 => "x64",
                CommonArch::Aarch64 => "aarch64",
            };

            let downloaded_artifact =
                ctx.reqv(|v| flowey_lib_common::download_gh_artifact::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "openvmm".into(),
                    file_name: format!("{arch_str}-openhcl-igvm"),
                    path: v,
                    run_id: run_id.clone(),
                });
            let arch = request.arch;

            ctx.emit_rust_step("write to directory variables", |ctx| {
                let downloaded_artifact = downloaded_artifact.claim(ctx);
                let write_release_output = output.claim(ctx);

                move |rt| {
                    let mut openhcl_direct = None;
                    let mut openhcl = None;
                    let mut openhcl_aarch64 = None;

                    match arch {
                        CommonArch::X86_64 => {
                            // x64 build contains both openhcl.bin and openhcl-direct.bin
                            let x64_dir = rt.read(downloaded_artifact).join("x64-openhcl-igvm");
                            openhcl_direct = Some(x64_dir.join("openhcl-direct.bin"));
                            openhcl = Some(x64_dir.join("openhcl.bin"));
                        }
                        CommonArch::Aarch64 => {
                            let aarch64_dir =
                                rt.read(downloaded_artifact).join("aarch64-openhcl-igvm");
                            openhcl_aarch64 = Some(aarch64_dir.join("openhcl-aarch64.bin"));
                        }
                    }

                    rt.write_not_secret(
                        write_release_output,
                        &ReleaseOutput {
                            openhcl_direct,
                            openhcl,
                            openhcl_aarch64,
                        },
                    );

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
