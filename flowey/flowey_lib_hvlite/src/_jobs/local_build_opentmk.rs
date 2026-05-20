// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that supports the `cargo xflowey build-opentmk` CLI

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,

        pub arch: crate::run_cargo_build::common::CommonArch,
        pub release: bool,
        /// Custom name for the output binary and VHD. Defaults to "opentmk".
        pub name: Option<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_opentmk::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            artifact_dir,
            done,
            arch,
            release,
            name,
        } = request;

        let profile = if release {
            crate::run_cargo_build::common::CommonProfile::Release
        } else {
            crate::run_cargo_build::common::CommonProfile::Debug
        };

        let name = name.unwrap_or_else(|| "opentmk".to_string());

        let opentmk_output = ctx.reqv(|v| crate::build_opentmk::Request {
            arch,
            profile,
            out_name: Some(name.clone()),
            opentmk: v,
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("package opentmk into VHD", |ctx| {
            done.claim(ctx);
            let artifact_dir = artifact_dir.claim(ctx);
            let opentmk_output = opentmk_output.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let crate::build_opentmk::OpentmkOutput { efi, pdb } = rt.read(opentmk_output);

                let output_dir = rt.read(artifact_dir);
                let output_dir = output_dir.absolute()?;
                fs_err::create_dir_all(&output_dir)?;

                // Step 1: Use xtask to create a raw GPT+EFI .img disk
                let img_path = output_dir.join(format!("{name}.img"));
                let arch_arg = match arch {
                    crate::run_cargo_build::common::CommonArch::X86_64 => "bootx64",
                    crate::run_cargo_build::common::CommonArch::Aarch64 => "bootaa64",
                };
                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(path);
                flowey::shell_cmd!(
                    rt,
                    "cargo xtask guest-test uefi --output {img_path} --{arch_arg} {efi}"
                )
                .run()?;

                // Step 2: Convert the raw .img to a fixed VHD1 by appending a
                // 512-byte footer. VHD1 fixed format = raw disk || footer.
                let vhd_path = output_dir.join(format!("{name}.vhd"));
                let disk_size = fs_err::metadata(&img_path)?.len();

                let footer = make_vhd1_fixed_footer(disk_size);
                fs_err::copy(&img_path, &vhd_path)?;
                let mut f = fs_err::OpenOptions::new().append(true).open(&vhd_path)?;
                std::io::Write::write_all(&mut f, &footer)?;

                // Clean up the intermediate .img
                let _ = fs_err::remove_file(&img_path);

                // Copy EFI, PDB to output dir
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

/// Build a 512-byte VHD1 fixed-disk footer.
///
/// The VHD1 fixed format is: raw disk bytes followed by a 512-byte footer.
/// See the VHD specification for field definitions.
fn make_vhd1_fixed_footer(disk_size: u64) -> [u8; 512] {
    let mut footer = [0u8; 512];

    // cookie: "conectix"
    footer[0..8].copy_from_slice(b"conectix");
    // features: 0x00000002
    footer[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes());
    // file format version: 0x00010000
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    // data offset: 0xFFFFFFFFFFFFFFFF (fixed disk)
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
    // time stamp: 0 (not critical)
    // creator application: "ovmm"
    footer[28..32].copy_from_slice(b"ovmm");
    // creator version: 0x000a0000
    footer[32..36].copy_from_slice(&0x000a_0000u32.to_be_bytes());
    // creator host OS: 0 (unknown)
    // original size
    footer[40..48].copy_from_slice(&disk_size.to_be_bytes());
    // current size
    footer[48..56].copy_from_slice(&disk_size.to_be_bytes());
    // disk geometry (CHS) — use a simple calculation
    let total_sectors = disk_size / 512;
    let (cylinders, heads, sectors_per_track) = compute_chs(total_sectors);
    let geom = ((cylinders as u32) << 16) | ((heads as u32) << 8) | (sectors_per_track as u32);
    footer[56..60].copy_from_slice(&geom.to_be_bytes());
    // disk type: 2 (fixed)
    footer[60..64].copy_from_slice(&2u32.to_be_bytes());
    // unique id: generate 16 random bytes using OS-seeded RandomState
    let mut unique_id = [0u8; 16];
    for chunk in unique_id.chunks_mut(8) {
        use std::hash::BuildHasher as _;
        use std::hash::Hasher as _;
        let h = std::hash::RandomState::new().build_hasher().finish();
        chunk.copy_from_slice(&h.to_ne_bytes());
    }
    footer[68..84].copy_from_slice(&unique_id);

    // checksum: one's complement of the sum of all bytes (excluding checksum field)
    let sum: u32 = footer
        .iter()
        .enumerate()
        .filter(|(i, _)| !(64..68).contains(i))
        .map(|(_, b)| *b as u32)
        .sum();
    let checksum = !sum;
    footer[64..68].copy_from_slice(&checksum.to_be_bytes());

    footer
}

/// Compute CHS geometry from total sector count, per the VHD spec.
fn compute_chs(total_sectors: u64) -> (u16, u8, u8) {
    let total_sectors = total_sectors.min(65535 * 16 * 255);

    if total_sectors >= 65535 * 16 * 63 {
        return (65535, 16, 255);
    }

    let mut sectors_per_track = 17u32;
    let mut heads = (total_sectors as u32 / sectors_per_track).div_ceil(1024);

    if heads < 4 {
        heads = 4;
    }
    if total_sectors as u32 >= heads * sectors_per_track * 1024 || heads > 16 {
        sectors_per_track = 31;
        heads = 16;
        if total_sectors as u32 >= heads * sectors_per_track * 1024 {
            sectors_per_track = 63;
            heads = 16;
        }
    }

    let cylinders = total_sectors as u32 / (heads * sectors_per_track);
    (
        cylinders.min(65535) as u16,
        heads as u8,
        sectors_per_track as u8,
    )
}
