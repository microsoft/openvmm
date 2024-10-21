// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `vmgs_lib` dynamic library + debug symbols.
//!
//! Content varies depending on what platform `vmgs_lib` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_and_test_vmgs_lib::VmgsLibOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub vmgs_lib: ReadVar<VmgsLibOutput>,
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
                vmgs_lib,
                artifact_dir,
                done,
            } = request;

            let files = vmgs_lib.map(ctx, |o| match o {
                VmgsLibOutput::LinuxDynamicLib { so } => {
                    vec![("libvmgs_lib.so".into(), so)]
                }
                VmgsLibOutput::WindowsDynamicLib { dll, dll_lib, pdb } => vec![
                    ("vmgs_lib.dll".into(), dll),
                    ("vmgs_lib.dll.lib".into(), dll_lib),
                    ("vmgs_lib.pdb".into(), pdb),
                ],
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "vmgs_lib".into(),
                files,
                artifact_dir,
                done,
            });

            Ok(())
        }
    }
}
