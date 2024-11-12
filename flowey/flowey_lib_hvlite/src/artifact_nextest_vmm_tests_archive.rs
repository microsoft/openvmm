// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: cargo-nextest archive file containing VMM tests.
//!
//! Content varies depending on what platform the VMM tests were compiled for.

const NEXTEST_VMM_TESTS_ARCHIVE_FILENAME: &str = "vmm_tests.tar.zst";

/// Publish the artifact.
pub mod publish {
    use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub vmm_tests: ReadVar<NextestVmmTestsArchive>,
            pub artifact_dir: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::copy_to_artifact_dir::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                vmm_tests,
                artifact_dir,
                done,
            } = request;

            let files = vmm_tests.map(ctx, |NextestVmmTestsArchive(path)| {
                vec![(super::NEXTEST_VMM_TESTS_ARCHIVE_FILENAME.into(), path)]
            });
            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "vmm_tests".into(),
                files,
                artifact_dir,
                done,
            });

            Ok(())
        }
    }
}

/// Resolve the contents of an existing artifact.
pub mod resolve {
    use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub nextest_archive: WriteVar<NextestVmmTestsArchive>,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::copy_to_artifact_dir::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                artifact_dir,
                nextest_archive,
            } = request;

            ctx.emit_rust_step("resolve vmm tests archive artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let nextest_archive = nextest_archive.claim(ctx);

                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);
                    let archive_file = artifact_dir.join(super::NEXTEST_VMM_TESTS_ARCHIVE_FILENAME);

                    if !archive_file.exists() {
                        anyhow::bail!(
                            "malformed artifact! did not contain {}",
                            super::NEXTEST_VMM_TESTS_ARCHIVE_FILENAME
                        )
                    }

                    rt.write(nextest_archive, &NextestVmmTestsArchive(archive_file));

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
