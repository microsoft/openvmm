// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct ReleaseOutput {
    #[serde(rename = "release-x64-openhcl-direct.bin")]
    pub x64_direct_bin: PathBuf,
    #[serde(rename = "release-x64-openhcl.bin")]
    pub x64_bin: PathBuf,
    #[serde(rename = "release-aarch64-openhcl.bin")]
    pub aarch64_bin: PathBuf,
}

impl Artifact for ReleaseOutput {}

#[derive(Serialize, Deserialize, Clone, Debug, Copy)]
pub enum OpenhclReleaseVersion {
    Release2411,
    Release2505,
}

impl OpenhclReleaseVersion {
    pub fn branch_name(&self) -> String {
        match self {
            OpenhclReleaseVersion::Release2411 => "release/2411".to_string(),
            OpenhclReleaseVersion::Release2505 => "release/2505".to_string(),
        }
    }

    pub const ALL: [OpenhclReleaseVersion; 2] = [
        OpenhclReleaseVersion::Release2411,
        OpenhclReleaseVersion::Release2505,
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
            pub test_content_dir: ReadVar<PathBuf>,
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
            let Request {
                release_igvm_files,
                release_version,
                test_content_dir,
            } = request;
            let branch_name: ReadVar<String> = ReadVar::from_static(release_version.branch_name());

            let run_id =
                ctx.reqv(
                    |v| flowey_lib_common::gh_latest_completed_workflow_id::Request {
                        repo: "microsoft/openvmm".into(),
                        branch: branch_name.clone(),
                        pipeline_name: "openvmm-ci.yaml".into(),
                        gh_workflow_id: v,
                    },
                );

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
                    file_name: format!("{arch_str}-openhcl-igvm"),
                    path: v,
                    run_id: run_id.clone(),
                });

                if arch == CommonArch::X86_64 {
                    downloaded_x64 = Some(downloaded);
                } else {
                    downloaded_aarch64 = Some(downloaded);
                }
            }

            let release_version_str = release_version.to_string();

            ctx.emit_rust_step("write to directory variables", |ctx| {
                let downloaded_x64 = downloaded_x64.unwrap().claim(ctx);
                let downloaded_aarch64 = downloaded_aarch64.unwrap().claim(ctx);

                let write_release_output = release_igvm_files.claim(ctx);

                let test_content_dir = test_content_dir.claim(ctx);

                move |rt| {
                    let downloaded_x64 = rt.read(downloaded_x64).join("x64-openhcl-igvm");
                    let downloaded_aarch64 =
                        rt.read(downloaded_aarch64).join("aarch64-openhcl-igvm");

                    let test_content_dir = rt.read(test_content_dir);

                    fs_err::copy(
                        downloaded_aarch64.clone().join("openhcl-aarch64.bin"),
                        test_content_dir
                            .clone()
                            .join(format!("{}-aarch64-openhcl.bin", release_version_str)),
                    )?;

                    fs_err::copy(
                        downloaded_x64.clone().join("openhcl.bin"),
                        test_content_dir
                            .clone()
                            .join(format!("{}-x64-openhcl.bin", release_version_str)),
                    )?;

                    fs_err::copy(
                        downloaded_x64.clone().join("openhcl-direct.bin"),
                        test_content_dir
                            .clone()
                            .join(format!("{}-x64-direct-openhcl.bin", release_version_str)),
                    )?;

                    rt.write_not_secret(
                        write_release_output,
                        &ReleaseOutput {
                            x64_direct_bin: test_content_dir
                                .join(format!("{}-x64-direct-openhcl.bin", release_version_str)),
                            x64_bin: test_content_dir
                                .join(format!("{}-x64-openhcl.bin", release_version_str)),
                            aarch64_bin: test_content_dir
                                .join(format!("{}-aarch64-openhcl.bin", release_version_str)),
                        },
                    );

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
