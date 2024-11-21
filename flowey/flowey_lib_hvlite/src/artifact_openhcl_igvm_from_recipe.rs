// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: A collection of OpenHCL IGVM files.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;

// marked pub(crate), as this is shared with sibling _from_recipe artifacts
pub(crate) fn recipe_to_filename(flavor: &OpenhclIgvmRecipe) -> &str {
    match flavor {
        OpenhclIgvmRecipe::X64 => "openhcl",
        OpenhclIgvmRecipe::X64Devkern => "openhcl-dev",
        OpenhclIgvmRecipe::X64TestLinuxDirect => "openhcl-direct",
        OpenhclIgvmRecipe::X64TestLinuxDirectDevkern => "openhcl-direct-dev",
        OpenhclIgvmRecipe::X64Cvm => "openhcl-cvm",
        OpenhclIgvmRecipe::X64CvmDevkern => "openhcl-cvm-dev",
        OpenhclIgvmRecipe::X64Nested => "openhcl-nested",
        OpenhclIgvmRecipe::Aarch64 => "openhcl-aarch64",
        OpenhclIgvmRecipe::Aarch64Devkern => "openhcl-aarch64-dev",
        OpenhclIgvmRecipe::LocalOnlyCustom(_) => unreachable!(),
    }
}

// marked pub(crate), as this is shared with sibling _from_recipe artifacts
pub(crate) fn filename_to_recipe(filename: &str) -> Option<OpenhclIgvmRecipe> {
    let ret = match filename {
        "openhcl" => OpenhclIgvmRecipe::X64,
        "openhcl-dev" => OpenhclIgvmRecipe::X64Devkern,
        "openhcl-direct" => OpenhclIgvmRecipe::X64TestLinuxDirect,
        "openhcl-direct-dev" => OpenhclIgvmRecipe::X64TestLinuxDirectDevkern,
        "openhcl-cvm" => OpenhclIgvmRecipe::X64Cvm,
        "openhcl-cvm-dev" => OpenhclIgvmRecipe::X64CvmDevkern,
        "openhcl-aarch64" => OpenhclIgvmRecipe::Aarch64,
        "openhcl-aarch64-dev" => OpenhclIgvmRecipe::Aarch64Devkern,
        _ => return None,
    };

    Some(ret)
}

/// Publish the artifact.
pub mod publish {
    use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
    use crate::run_igvmfilegen::IgvmOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub openhcl_igvm_files: Vec<ReadVar<(OpenhclIgvmRecipe, IgvmOutput)>>,
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
                openhcl_igvm_files,
                artifact_dir,
                done,
            } = request;

            let files = ctx.emit_rust_stepv("describe OpenHCL igvm artifact", |ctx| {
                let openhcl_igvm_files = openhcl_igvm_files.claim(ctx);
                |rt| {
                    let mut files = Vec::new();
                    for igvm in openhcl_igvm_files {
                        let (recipe, igvm) = rt.read(igvm);

                        let IgvmOutput {
                            igvm_bin,
                            igvm_tdx_json,
                            igvm_snp_json,
                            igvm_vbs_json,
                            ..
                        } = igvm;
                        files.push((
                            format!("{}.bin", super::recipe_to_filename(&recipe)).into(),
                            igvm_bin,
                        ));

                        if let Some(igvm_tdx_json) = igvm_tdx_json {
                            files.push((
                                format!("{}-tdx.json", super::recipe_to_filename(&recipe)).into(),
                                igvm_tdx_json,
                            ));
                        }

                        if let Some(igvm_snp_json) = igvm_snp_json {
                            files.push((
                                format!("{}-snp.json", super::recipe_to_filename(&recipe)).into(),
                                igvm_snp_json,
                            ));
                        }

                        if let Some(igvm_vbs_json) = igvm_vbs_json {
                            files.push((
                                format!("{}-vbs.json", super::recipe_to_filename(&recipe)).into(),
                                igvm_vbs_json,
                            ));
                        }
                    }
                    Ok(files)
                }
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "OpenHCL igvm files".into(),
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
    use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
    use crate::run_igvmfilegen::IgvmOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub igvm_files: WriteVar<Vec<(OpenhclIgvmRecipe, IgvmOutput)>>,
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
                igvm_files,
            } = request;

            ctx.emit_rust_step("resolve OpenHCL igvm artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let igvm_files = igvm_files.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let mut files = Vec::new();
                    for entry in fs_err::read_dir(&artifact_dir)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            anyhow::bail!("unexpected folder in root");
                        }

                        // For each binary in this directory, we create an IgvmOutput,
                        // and search for accompanying json endoresements to include from
                        // the same dir.
                        let entry_file_name = entry
                            .file_name()
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("unexpected filename"))?;
                        let entry_file_stem =
                            if let Some(file_stem) = entry_file_name.strip_suffix(".bin") {
                                file_stem
                            } else {
                                continue;
                            };

                        let Some(recipe) = super::filename_to_recipe(entry_file_stem) else {
                            anyhow::bail!(
                                "unexpected file in openhcl_igvm artifact folder: {}",
                                entry.path().display()
                            );
                        };

                        // For each openhcl bin, we search for corresponding tdx, snp, and vbs endorsements.
                        let igvm_path_file_name = super::recipe_to_filename(&recipe);
                        let igvm_tdx_json = {
                            let path = artifact_dir.join(format!("{igvm_path_file_name}-tdx.json"));
                            path.exists().then_some(path)
                        };
                        let igvm_snp_json = {
                            let path = artifact_dir.join(format!("{igvm_path_file_name}-snp.json"));
                            path.exists().then_some(path)
                        };
                        let igvm_vbs_json = {
                            let path = artifact_dir.join(format!("{igvm_path_file_name}-vbs.json"));
                            path.exists().then_some(path)
                        };

                        files.push((
                            recipe,
                            IgvmOutput {
                                igvm_bin: entry.path(),
                                igvm_map: None, // resolved through _extras
                                igvm_tdx_json,
                                igvm_snp_json,
                                igvm_vbs_json,
                            },
                        ))
                    }

                    rt.write(igvm_files, &files);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
