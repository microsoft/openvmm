// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Document crates in the hvlite repo using rustdoc (via `cargo doc`).

use crate::download_lxutil::LxutilArch;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_doc::DocPackage;
use flowey_lib_common::run_cargo_doc::DocPackageKind;

new_flow_node!(struct Node);

flowey_request! {
    pub enum Request {
        Doc {
            target_triple: target_lexicon::Triple,
            docs_dir: WriteVar<PathBuf>,
        }
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_lxutil::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<flowey_lib_common::run_cargo_doc::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut doc_requests = Vec::new();

        for req in requests {
            match req {
                Request::Doc {
                    target_triple,
                    docs_dir,
                } => doc_requests.push((target_triple, docs_dir)),
            }
        }

        let doc_requests = doc_requests;

        // -- end of req processing -- //

        if doc_requests.is_empty() {
            return Ok(());
        }

        let side_effects = vec![ctx.reqv(crate::install_openvmm_rust_build_essential::Request)];

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let no_deps = true;
        let document_private_items = false; // TODO: would be nice to turn this on

        for (target_triple, docs_dir) in doc_requests {
            let mut target_side_effects = side_effects.clone();

            // lxutil is required by certain build.rs scripts.
            //
            // FUTURE: should prob have a way to opt-out of this lxutil build
            // script requirement in non-interactive scenarios?
            let lxutil_arch = match target_triple.architecture {
                target_lexicon::Architecture::X86_64 => LxutilArch::X86_64,
                target_lexicon::Architecture::Aarch64(_) => LxutilArch::Aarch64,
                arch => anyhow::bail!("unsupported arch {arch}"),
            };
            target_side_effects.push(ctx.reqv(|v| crate::init_openvmm_magicpath_lxutil::Request {
                arch: lxutil_arch,
                done: v,
            }));

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
                target_side_effects.to_vec().claim(ctx);
                let docs_dir = docs_dir.claim(ctx);
                let cargo_cmd = cargo_cmd.claim(ctx);
                move |rt| {
                    let cargo_cmd = rt.read(cargo_cmd);
                    let sh = xshell::Shell::new()?;
                    let out_path = cargo_cmd.run(&sh)?;

                    rt.write(docs_dir, &out_path);
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
