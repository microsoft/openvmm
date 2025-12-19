// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Raw bindings to `igvmfilegen`, used to build an igvm file from a manifest +
//! set of resources.

use flowey::node::prelude::*;
use igvmfilegen_config::ResourceType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct IgvmOutput {
    pub igvm_bin: PathBuf,
    pub igvm_map: Option<PathBuf>,
    pub igvm_tdx_json: Option<PathBuf>,
    pub igvm_snp_json: Option<PathBuf>,
    pub igvm_vbs_json: Option<PathBuf>,
}

flowey_request! {
    pub struct Request {
        /// Path to igvmfilegen bin to use
        pub igvmfilegen: ReadVar<PathBuf>,
        /// IGVM manifest to build
        pub manifest: ReadVar<PathBuf>,
        /// Resources required by the provided IGVM manifest
        pub resources: ReadVar<BTreeMap<ResourceType, PathBuf>>,
        /// Output path of generated igvm file
        pub igvm: WriteVar<IgvmOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            igvmfilegen,
            manifest,
            resources,
            igvm,
        } = request;

        ctx.emit_rust_step("building igvm file", |ctx| {
            let igvm = igvm.claim(ctx);
            let igvmfilegen = igvmfilegen.claim(ctx);
            let manifest = manifest.claim(ctx);
            let resources = resources.claim(ctx);
            move |rt| {
                let igvmfilegen = rt.read(igvmfilegen);
                let manifest = rt.read(manifest);
                let resources = rt.read(resources);

                let sh = xshell::Shell::new()?;

                let igvm_file_stem = "igvm";
                let igvm_path = sh.current_dir().join(format!("{igvm_file_stem}.bin"));
                let resources_path = sh.current_dir().join("igvm.json");

                let resources = igvmfilegen_config::Resources::new(resources.into_iter().collect())
                    .context("creating igvm resources")?;
                std::fs::write(&resources_path, serde_json::to_string_pretty(&resources)?)
                    .context("writing resources")?;

                xshell::cmd!(
                    sh,
                    "{igvmfilegen} manifest
                            -m {manifest}
                            -r {resources_path}
                            --debug-validation
                            -o {igvm_path}
                        "
                )
                .run()?;

                let igvm_map_path = igvm_path.with_extension("bin.map");
                let igvm_map_path = igvm_map_path.exists().then_some(igvm_map_path);
                let igvm_tdx_json = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-tdx.json"));
                    path.exists().then_some(path)
                };
                let igvm_snp_json = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-snp.json"));
                    path.exists().then_some(path)
                };
                let igvm_vbs_json = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-vbs.json"));
                    path.exists().then_some(path)
                };

                rt.write(
                    igvm,
                    &IgvmOutput {
                        igvm_bin: igvm_path,
                        igvm_map: igvm_map_path,
                        igvm_tdx_json,
                        igvm_snp_json,
                        igvm_vbs_json,
                    },
                );

                Ok(())
            }
        });

        Ok(())
    }
}
