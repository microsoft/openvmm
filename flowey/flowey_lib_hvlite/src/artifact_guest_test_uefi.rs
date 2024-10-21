// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `guest_test_uefi.img` disk image, along with contained
//! `guest_test_uefi.efi` UEFI executable (with symbols).

/// Publish the artifact.
pub mod publish {
    use crate::build_guest_test_uefi::GuestTestUefiOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub guest_test_uefi: ReadVar<GuestTestUefiOutput>,
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
                guest_test_uefi,
                artifact_dir,
                done,
            } = request;

            let files = guest_test_uefi.map(ctx, |GuestTestUefiOutput { efi, pdb, img }| {
                vec![
                    ("guest_test_uefi.efi".into(), efi),
                    ("guest_test_uefi.pdb".into(), pdb),
                    ("guest_test_uefi.img".into(), img),
                ]
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "guest_test_uefi".into(),
                artifact_dir,
                files,
                done,
            });

            Ok(())
        }
    }
}

/// Resolve the contents of an existing artifact.
pub mod resolve {
    use crate::build_guest_test_uefi::GuestTestUefiOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub guest_test_uefi: WriteVar<GuestTestUefiOutput>,
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
                guest_test_uefi,
            } = request;

            ctx.emit_rust_step("resolve guest_test_uefi artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let guest_test_uefi = guest_test_uefi.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    for ext in ["efi", "pdb", "img"] {
                        if !artifact_dir.join(format!("guest_test_uefi.{ext}")).exists() {
                            anyhow::bail!("malformed artifact! did not find guest_test_uefi.{ext}");
                        }
                    }

                    let output = GuestTestUefiOutput {
                        efi: artifact_dir.join("guest_test_uefi.efi"),
                        pdb: artifact_dir.join("guest_test_uefi.pdb"),
                        img: artifact_dir.join("guest_test_uefi.img"),
                    };

                    rt.write(guest_test_uefi, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
