// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `openhcl` binary and kernel binaries to use for PR binary size comparison

/// Publish the artifact.
pub mod publish {
    use crate::build_openvmm_hcl::OpenvmmHclOutput;
    use flowey::node::prelude::*;

    /// A kernel binary to include in the sizecheck artifact.
    #[derive(Serialize, Deserialize)]
    pub struct KernelBaseline {
        /// Label used as the filename in the artifact dir (e.g., "kernel-main").
        pub label: String,
        /// Path to the kernel binary.
        pub kernel: ReadVar<PathBuf>,
    }

    flowey_request! {
        pub struct Request {
            pub openvmm_openhcl: ReadVar<OpenvmmHclOutput>,
            pub kernel_baselines: Vec<KernelBaseline>,
            pub artifact_dir: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(_ctx: &mut ImportCtx<'_>) {}

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                openvmm_openhcl,
                kernel_baselines,
                artifact_dir,
                done,
            } = request;

            ctx.emit_rust_step("copying openhcl build and kernels to publish dir", |ctx| {
                done.claim(ctx);
                let artifact_dir = artifact_dir.claim(ctx);
                let openvmm_openhcl = openvmm_openhcl.claim(ctx);
                let kernel_baselines: Vec<_> = kernel_baselines
                    .into_iter()
                    .map(|kb| (kb.label, kb.kernel.claim(ctx)))
                    .collect();

                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);
                    let openvmm_openhcl = rt.read(openvmm_openhcl);
                    fs_err::copy(openvmm_openhcl.bin, artifact_dir.join("openhcl"))?;

                    let mut seen_labels = std::collections::HashSet::new();
                    for (label, kernel_var) in kernel_baselines {
                        anyhow::ensure!(
                            !label.is_empty()
                                && !label.contains('/')
                                && !label.contains('\\')
                                && !label.starts_with('.'),
                            "kernel baseline label must be a non-empty simple filename: {label}"
                        );
                        anyhow::ensure!(
                            seen_labels.insert(label.clone()),
                            "duplicate kernel baseline label: {label}"
                        );
                        let kernel_path = rt.read(kernel_var);
                        fs_err::copy(kernel_path, artifact_dir.join(&label))?;
                    }

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
