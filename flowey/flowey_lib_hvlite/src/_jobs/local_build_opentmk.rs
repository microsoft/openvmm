// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that supports the `cargo xflowey build-opentmk` CLI

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,

        pub arch: crate::common::CommonArch,
        pub release: bool,
        /// Custom name for the output `.efi`, `.vhd`, and `.pdb`. Defaults to "opentmk".
        pub name: Option<String>,
        /// Optional JSON test-selection config to embed in the built VHD.
        pub config: Option<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_opentmk::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            artifact_dir,
            done,
            arch,
            release,
            name,
            config,
        } = request;

        let profile = if release {
            crate::common::CommonProfile::Release
        } else {
            crate::common::CommonProfile::Debug
        };

        let name = name.unwrap_or_else(|| "opentmk".to_string());

        let opentmk_output = ctx.reqv(|v| crate::build_opentmk::Request {
            arch,
            profile,
            out_name: Some(name.clone()),
            opentmk: v,
        });

        ctx.emit_rust_step("package opentmk into VHD", |ctx| {
            done.claim(ctx);
            let artifact_dir = artifact_dir.claim(ctx);
            let opentmk_output = opentmk_output.claim(ctx);
            move |rt| {
                let crate::build_opentmk::OpentmkOutput { efi, pdb } = rt.read(opentmk_output);

                let output_dir = rt.read(artifact_dir);
                let output_dir = output_dir.absolute()?;
                fs_err::create_dir_all(&output_dir)?;

                // Build the bootable VHD, patching in the test config if one was given.
                let disk_arch = match arch {
                    crate::common::CommonArch::X86_64 => opentmk_disk::Arch::X86_64,
                    crate::common::CommonArch::Aarch64 => opentmk_disk::Arch::Aarch64,
                };
                let efi_bytes = fs_err::read(&efi)?;
                let vhd_path = output_dir.join(format!("{name}.vhd"));
                let image = if let Some(cfg_path) = &config {
                    let config_json = fs_err::read(cfg_path.absolute()?)?;
                    opentmk_disk::build_opentmk_vhd_with_config(
                        &efi_bytes,
                        disk_arch,
                        &config_json,
                    )?
                } else {
                    opentmk_disk::build_opentmk_vhd(&efi_bytes, disk_arch)?
                };
                image.persist(&vhd_path)?;

                fs_err::copy(&efi, output_dir.join(format!("{name}.efi")))?;
                fs_err::copy(&pdb, output_dir.join(format!("{name}.pdb")))?;

                log::info!("EFI: {}", output_dir.join(format!("{name}.efi")).display());
                log::info!("VHD: {}", vhd_path.display());
                log::info!("PDB: {}", output_dir.join(format!("{name}.pdb")).display());

                Ok(())
            }
        });

        Ok(())
    }
}
