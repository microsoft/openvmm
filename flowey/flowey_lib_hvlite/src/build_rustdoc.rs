// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Document crates in the hvlite repo using rustdoc (via `cargo doc`).

use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_doc::DocPackage;
use flowey_lib_common::run_cargo_doc::DocPackageKind;

new_flow_node!(struct Node);

flowey_request! {
    pub struct Request {
        pub target_triple: target_lexicon::Triple,
        pub docs: WriteVar<RustdocOutput>,
    }
}

#[derive(Serialize, Deserialize)]
pub struct RustdocOutput {
    pub docs: PathBuf,
}

impl Artifact for RustdocOutput {
    // The rustdoc output has too many files for Azure DevOps to handle,
    // so we need to archive it before uploading.
    const TAR_GZ_NAME: Option<&'static str> = Some("rustdoc.tar.gz");
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<flowey_lib_common::run_cargo_doc::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut doc_requests = Vec::new();

        for Request {
            target_triple,
            docs,
        } in requests
        {
            doc_requests.push((target_triple, docs));
        }

        let doc_requests = doc_requests;

        // -- end of req processing -- //

        if doc_requests.is_empty() {
            return Ok(());
        }

        let side_effects = [ctx.reqv(crate::install_openvmm_rust_build_essential::Request)];

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let no_deps = true;
        let document_private_items = false; // TODO: would be nice to turn this on

        for (target_triple, output) in doc_requests {
            let cargo_cmd = ctx.reqv(|v| {
                flowey_lib_common::run_cargo_doc::Request {
                    in_folder: openvmm_repo_path.clone(),
                    packages: vec![
                        DocPackage {
                            kind: DocPackageKind::Workspace {
                                // this is a bin crate with no interesting docs;
                                // easier to just exclude it.
                                exclude: vec!["vmfirmwareigvm_dll".into()],
                            },
                            no_deps,
                            document_private_items,
                        },
                        DocPackage {
                            kind: DocPackageKind::NoStdCrate("guest_test_uefi".into()),
                            no_deps,
                            document_private_items,
                        },
                    ],
                    target_triple: target_triple.clone(),
                    cargo_cmd: v,
                }
            });

            ctx.emit_rust_step(format!("document repo for target {target_triple}"), |ctx| {
                side_effects.to_vec().claim(ctx);
                let output = output.claim(ctx);
                let cargo_cmd = cargo_cmd.claim(ctx);
                move |rt| {
                    let cargo_cmd = rt.read(cargo_cmd);
                    let out_path = cargo_cmd.run(rt)?;

                    rt.write(output, &RustdocOutput { docs: out_path });
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
