// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: An artifact containing various "extras" that are generated as part
//! of the OpenHCL IGVM build. e.g: debug symbols, constituent binaries, etc.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct OpenhclIgvmExtras {
    pub recipe: OpenhclIgvmRecipe,
    pub openvmm_hcl_bin: crate::build_openvmm_hcl::OpenvmmHclOutput,
    pub openhcl_map: Option<PathBuf>,
    pub openhcl_boot: crate::build_openhcl_boot::OpenhclBootOutput,
    pub sidecar: Option<crate::build_sidecar::SidecarOutput>,
}

/// Publish the artifact.
pub mod publish {
    use super::OpenhclIgvmExtras;
    use crate::build_openhcl_boot::OpenhclBootOutput;
    use crate::build_openvmm_hcl::OpenvmmHclOutput;
    use crate::build_sidecar::SidecarOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub extras: Vec<ReadVar<OpenhclIgvmExtras>>,
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
                extras,
                artifact_dir,
                done,
            } = request;

            let files = ctx.emit_minor_rust_stepv("describe OpenHCL igvm extras artifact", |ctx| {
                let extras = extras.claim(ctx);
                |rt| {
                    let mut files = Vec::new();
                    for extra in extras {
                        let OpenhclIgvmExtras {
                            recipe,
                            openvmm_hcl_bin,
                            openhcl_map,
                            openhcl_boot,
                            sidecar,
                        } = rt.read(extra);

                        let folder_name =
                            crate::artifact_openhcl_igvm_from_recipe::recipe_to_filename(&recipe);

                        {
                            let OpenvmmHclOutput { bin, dbg } = openvmm_hcl_bin;
                            files.push((format!("{folder_name}/openvmm_hcl").into(), bin));
                            if let Some(dbg) = dbg {
                                files.push((format!("{folder_name}/openvmm_hcl.dbg").into(), dbg));
                            }
                        }

                        if let Some(map) = openhcl_map {
                            files.push((format!("{folder_name}/openhcl.bin.map").into(), map));
                        }

                        {
                            let OpenhclBootOutput { bin, dbg } = openhcl_boot;
                            files.push((format!("{folder_name}/openhcl_boot").into(), bin));
                            files.push((format!("{folder_name}/openhcl_boot.dbg").into(), dbg));
                        }

                        if let Some(SidecarOutput { bin, dbg }) = sidecar {
                            files.push((format!("{folder_name}/sidecar").into(), bin));
                            files.push((format!("{folder_name}/sidecar.dbg").into(), dbg));
                        }
                    }
                    files
                }
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "OpenHCL igvm extras".into(),
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
    use super::OpenhclIgvmExtras;
    use crate::build_openhcl_boot::OpenhclBootOutput;
    use crate::build_openvmm_hcl::OpenvmmHclOutput;
    use crate::build_sidecar::SidecarOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub extras: WriteVar<Vec<OpenhclIgvmExtras>>,
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
                extras,
            } = request;

            ctx.emit_rust_step("resolve OpenHCL igvm extras artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let extras = extras.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let mut files = Vec::new();
                    for entry in fs_err::read_dir(artifact_dir)? {
                        let entry = entry?;

                        if !entry.file_type()?.is_dir() {
                            anyhow::bail!("unexpected file in root: {:?}", entry.file_name());
                        }

                        // deny auto-ingest of custom igvm files.
                        //
                        // by defn: a "custom" igvm file is no longer "custom"
                        // when it gets auto-ingested by another job. You get
                        // into a world of matching filename string, and lose a
                        // bunch of type-safety...
                        let recipe = crate::artifact_openhcl_igvm_from_recipe::filename_to_recipe(
                            entry
                                .file_name()
                                .to_str()
                                .context("unexpected folder name")?,
                        )
                        .context("unexpected folder name")?;

                        let folder_name = entry.path();
                        files.push(OpenhclIgvmExtras {
                            recipe,
                            openvmm_hcl_bin: OpenvmmHclOutput {
                                bin: folder_name.join("openhcl"),
                                dbg: {
                                    let p = folder_name.join("openvmm_hcl.dbg");
                                    p.exists().then_some(p)
                                },
                            },
                            openhcl_map: {
                                let p = folder_name.join("openhcl.bin.map");
                                p.exists().then_some(p)
                            },
                            openhcl_boot: OpenhclBootOutput {
                                bin: folder_name.join("openhcl_boot"),
                                dbg: folder_name.join("openhcl_boot.dbg"),
                            },
                            sidecar: Some(SidecarOutput {
                                bin: folder_name.join("sidecar"),
                                dbg: folder_name.join("sidecar.dbg"),
                            }),
                        })
                    }

                    rt.write(extras, &files);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
