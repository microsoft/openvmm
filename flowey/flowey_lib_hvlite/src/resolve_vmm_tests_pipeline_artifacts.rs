// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve artifacts from the test content dir used as parts of the pipeline
//! to run VMM tests.

use crate::build_incubator::IncubatorOutput;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_prep_steps::PrepStepsOutput;
use crate::build_test_igvm_agent_rpc_server::TestIgvmAgentRpcServerOutput;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct VmmTestsBuiltPipelineArtifacts {}

flowey_request! {
    pub struct Request {
        /// Directory to symlink / copy test contents into. Does not need to be
        /// empty.
        pub test_content_dir: ReadVar<PathBuf>,
        /// What triple VMM tests are built for.
        ///
        /// Used to detect cases of running Windows VMM tests via WSL2, and adjusting
        /// reported paths appropriately.
        pub vmm_tests_target: target_lexicon::Triple,

        // Artifacts used by the test pipeline that are built in the openvmm repo
        pub nextest_vmm_tests_archive: WriteVar<NextestVmmTestsArchive>,
        pub incubator: Option<WriteVar<IncubatorOutput>>,
        pub prep_steps: Option<WriteVar<PrepStepsOutput>>,
        pub test_igvm_agent_rpc_server: Option<WriteVar<TestIgvmAgentRpcServerOutput>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            vmm_tests_target,
            nextest_vmm_tests_archive,
            incubator,
            prep_steps,
            test_igvm_agent_rpc_server,
        } = request;

        let windows_target = matches!(
            vmm_tests_target.operating_system,
            target_lexicon::OperatingSystem::Windows
        );

        ctx.emit_rust_step("resolving vmm tests pipeline artifacts", |ctx| {
            claim_vars!(
                ctx,
                (
                    test_content_dir,
                    nextest_vmm_tests_archive,
                    incubator,
                    prep_steps,
                    test_igvm_agent_rpc_server,
                )
            );

            move |rt| {
                let test_content_dir = rt.read(test_content_dir);

                let nextest_vmm_tests_archive_path = test_content_dir.join("vmm_tests.tar.zst");
                if nextest_vmm_tests_archive_path.exists() {
                    rt.write(
                        nextest_vmm_tests_archive,
                        &NextestVmmTestsArchive {
                            archive_file: nextest_vmm_tests_archive_path,
                        },
                    );
                } else {
                    anyhow::bail!("{} not found", nextest_vmm_tests_archive_path.display());
                }

                if let Some(incubator) = incubator {
                    if windows_target {
                        anyhow::bail!("incubator is linux only");
                    }
                    let incubator_path = test_content_dir.join("incubator");
                    if incubator_path.exists() {
                        rt.write(
                            incubator,
                            &IncubatorOutput {
                                bin: incubator_path,
                                dbg: None,
                            },
                        );
                    } else {
                        anyhow::bail!("{} not found", incubator_path.display());
                    }
                }

                if let Some(prep_steps) = prep_steps {
                    let prep_steps_path = test_content_dir.join(if windows_target {
                        "prep_steps.exe"
                    } else {
                        "prep_steps"
                    });
                    if prep_steps_path.exists() {
                        let prep_steps_output = if windows_target {
                            PrepStepsOutput::WindowsBin {
                                exe: prep_steps_path,
                                pdb: None,
                            }
                        } else {
                            PrepStepsOutput::LinuxBin {
                                bin: prep_steps_path,
                                dbg: None,
                            }
                        };
                        rt.write(prep_steps, &prep_steps_output);
                    } else {
                        anyhow::bail!("{} not found", prep_steps_path.display());
                    };
                }

                if let Some(test_igvm_agent_rpc_server) = test_igvm_agent_rpc_server {
                    if !windows_target {
                        anyhow::bail!("test_igvm_agent_rpc_server is windows only");
                    }
                    let test_igvm_agent_rpc_server_path =
                        test_content_dir.join("test_igvm_agent_rpc_server.exe");
                    if test_igvm_agent_rpc_server_path.exists() {
                        rt.write(
                            test_igvm_agent_rpc_server,
                            &TestIgvmAgentRpcServerOutput {
                                exe: test_igvm_agent_rpc_server_path,
                                pdb: None,
                            },
                        );
                    } else {
                        anyhow::bail!("{} not found", test_igvm_agent_rpc_server_path.display());
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
