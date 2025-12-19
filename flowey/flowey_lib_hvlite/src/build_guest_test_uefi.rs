// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `guest_test_uefi` images and binaries

use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct GuestTestUefiOutput {
    #[serde(rename = "guest_test_uefi.efi")]
    pub efi: PathBuf,
    #[serde(rename = "guest_test_uefi.pdb")]
    pub pdb: PathBuf,
    #[serde(rename = "guest_test_uefi.img")]
    pub img: PathBuf,
}

impl Artifact for GuestTestUefiOutput {}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub profile: CommonProfile,
        pub guest_test_uefi: WriteVar<GuestTestUefiOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut tasks: BTreeMap<_, Vec<_>> = BTreeMap::new();

        for Request {
            arch,
            profile,
            guest_test_uefi,
        } in requests
        {
            tasks
                .entry((arch, profile))
                .or_default()
                .push(guest_test_uefi);
        }

        for ((arch, profile), outvars) in tasks {
            let output = ctx.reqv(|v| {
                crate::run_cargo_build::Request {
                    crate_name: "guest_test_uefi".into(),
                    out_name: "guest_test_uefi".into(),
                    crate_type: CargoCrateType::Bin,
                    profile: profile.into(),
                    features: Default::default(),
                    target: target_lexicon::Triple {
                        architecture: arch.as_arch(),
                        operating_system: target_lexicon::OperatingSystem::Uefi,
                        environment: target_lexicon::Environment::Unknown,
                        vendor: target_lexicon::Vendor::Unknown,
                        // work around bug in target_lexicon (this shouldn't be Elf)
                        binary_format: target_lexicon::BinaryFormat::Elf,
                    },
                    no_split_dbg_info: false,
                    extra_env: None,
                    pre_build_deps: Vec::new(),
                    output: v,
                }
            });

            let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

            ctx.emit_rust_step("build guest_test_uefi.img", |ctx| {
                let openvmm_repo_path = openvmm_repo_path.claim(ctx);
                let output = output.claim(ctx);
                let outvars = outvars.claim(ctx);
                move |rt| {
                    let (efi, pdb) = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::UefiBin { efi, pdb } => {
                            (efi, pdb)
                        }
                        _ => unreachable!(),
                    };

                    // package it up into an img using the xtask
                    let sh = xshell::Shell::new()?;
                    let img_path = sh.current_dir().join("guest_test_uefi.img");
                    let arch_arg = match arch {
                        CommonArch::X86_64 => "bootx64",
                        CommonArch::Aarch64 => "bootaa64",
                    };
                    sh.change_dir(rt.read(openvmm_repo_path));
                    xshell::cmd!(
                        sh,
                        "cargo xtask guest-test uefi --output {img_path} --{arch_arg} {efi}"
                    )
                    .run()?;

                    let output = GuestTestUefiOutput {
                        efi,
                        pdb,
                        img: img_path.absolute()?,
                    };

                    for var in outvars {
                        rt.write(var, &output);
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
