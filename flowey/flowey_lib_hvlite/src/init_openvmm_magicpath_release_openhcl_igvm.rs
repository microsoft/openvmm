// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod resolve {
    use crate::download_release_igvm_files_from_gh;
    use flowey::node::prelude::new_simple_flow_node;
    use flowey::node::prelude::*;

    new_simple_flow_node!(struct Node);

    flowey_request! {
        pub struct Request{
            pub release_version: download_release_igvm_files_from_gh::OpenhclReleaseVersion,
            pub release_artifact: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<download_release_igvm_files_from_gh::resolve::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                release_version,
                release_artifact,
                done,
            } = request;

            let latest_release_igvm_files =
                ctx.reqv(|v| download_release_igvm_files_from_gh::resolve::Request {
                    release_igvm_files: v,
                    release_version,
                });

            let release_version = release_version.to_string();
            let aarch64_name = format!("{}-aarch64-openhcl.bin", release_version);
            let x64_name = format!("{}-x64-openhcl.bin", release_version);
            let direct_name = format!("{}-x64-direct-openhcl.bin", release_version);

            ctx.emit_rust_step(
                "copy downloaded release igvm files to artifact dir",
                move |ctx| {
                    let latest_release_igvm_files = latest_release_igvm_files.claim(ctx);
                    let latest_release_artifact = release_artifact.claim(ctx);
                    done.claim(ctx);

                    move |rt| {
                        let latest_release_igvm_files = rt.read(latest_release_igvm_files);
                        let latest_release_artifact = rt.read(latest_release_artifact);

                        if let Some(src) = &latest_release_igvm_files.openhcl_aarch64 {
                            fs_err::copy(src, latest_release_artifact.join(&aarch64_name))?;
                        }

                        if let Some(src) = &latest_release_igvm_files.openhcl {
                            fs_err::copy(src, latest_release_artifact.join(&x64_name))?;
                        }

                        if let Some(src) = &latest_release_igvm_files.openhcl_direct {
                            fs_err::copy(src, latest_release_artifact.join(&direct_name))?;
                        }

                        Ok(())
                    }
                },
            );

            Ok(())
        }
    }
}
