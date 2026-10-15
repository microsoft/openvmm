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
    /// The unsigned SNP ID block signing payload (`<base>-snp.idblock`), if the
    /// manifest produced a measurable SEV-SNP platform.
    pub igvm_snp_idblock: Option<PathBuf>,
    pub igvm_tdx_corim: Option<PathBuf>,
    pub igvm_snp_corim: Option<PathBuf>,
    pub igvm_vbs_corim: Option<PathBuf>,
}

flowey_request! {
    pub struct Request {
        /// Path to igvmfilegen bin to use
        pub igvmfilegen: ReadVar<PathBuf>,
        /// IGVM manifest to build
        pub manifest: ReadVar<PathBuf>,
        /// Resources required by the provided IGVM manifest
        pub resources: ReadVar<BTreeMap<ResourceType, PathBuf>>,
        /// Whether to patch the manifest to set secure_avic to disabled
        pub disable_secure_avic: bool,
        /// Whether to add the confidential debug flag to the measured OpenHCL
        /// command line, enabling confidential diagnostics on CVM builds even
        /// in release builds.
        pub confidential_debug: bool,
        /// For SEV-SNP builds, add an SNP ID block signed by an ephemeral key
        /// (via `igvmfilegen add-snp-id-block --manifest`). This restores the
        /// pre-migration behavior for open-source builds, where every SNP IGVM
        /// carried a temporary-key ID block so that `id_block_en = 1` at launch.
        /// Production pipelines set this to `false` and instead add an ID block
        /// signed with a real key out-of-band.
        pub add_temp_snp_id_block: bool,
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
            disable_secure_avic,
            confidential_debug,
            add_temp_snp_id_block,
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

                let igvm_file_stem = "igvm";
                let igvm_path = rt.sh.current_dir().join(format!("{igvm_file_stem}.bin"));
                let resources_path = rt.sh.current_dir().join("igvm.json");

                let resources = igvmfilegen_config::Resources::new(resources.into_iter().collect())
                    .context("creating igvm resources")?;
                std::fs::write(&resources_path, serde_json::to_string_pretty(&resources)?)
                    .context("writing resources")?;

                let mut cmd = flowey::shell_cmd!(
                    rt,
                    "{igvmfilegen} manifest
                            -m {manifest}
                            -r {resources_path}
                            --debug-validation
                            -o {igvm_path}
                        "
                );

                if disable_secure_avic {
                    cmd = cmd.arg("--disable-secure-avic");
                }

                if confidential_debug {
                    cmd = cmd.arg("--confidential-debug");
                }

                cmd.run()?;

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
                let igvm_snp_idblock = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-snp.idblock"));
                    path.exists().then_some(path)
                };

                // For open-source SEV-SNP builds, embed an ID block signed by an
                // ephemeral key so the file launches with `id_block_en = 1`, as
                // it did before the ID block was split into a separate step.
                // The presence of `<stem>-snp.idblock` means the manifest built
                // a measurable SNP platform.
                if add_temp_snp_id_block && igvm_snp_idblock.is_some() {
                    flowey::shell_cmd!(
                        rt,
                        "{igvmfilegen} add-snp-id-block
                                --input {igvm_path}
                                --output {igvm_path}
                                --manifest {manifest}
                            "
                    )
                    .run()?;
                }
                let igvm_tdx_corim = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-tdx.cbor"));
                    path.exists().then_some(path)
                };
                let igvm_snp_corim = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-snp.cbor"));
                    path.exists().then_some(path)
                };
                let igvm_vbs_corim = {
                    let path = igvm_path.with_file_name(format!("{igvm_file_stem}-vbs.cbor"));
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
                        igvm_snp_idblock,
                        igvm_tdx_corim,
                        igvm_snp_corim,
                        igvm_vbs_corim,
                    },
                );

                Ok(())
            }
        });

        Ok(())
    }
}
