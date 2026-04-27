// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `opentmk` UEFI binaries

use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct OpentmkOutput {
    pub efi: PathBuf,
    pub pdb: PathBuf,
}

impl Artifact for OpentmkOutput {}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub profile: CommonProfile,
        /// Custom output binary name. Defaults to "opentmk".
        pub out_name: Option<String>,
        pub opentmk: WriteVar<OpentmkOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut tasks: BTreeMap<_, Vec<_>> = BTreeMap::new();

        for Request {
            arch,
            profile,
            out_name,
            opentmk,
        } in requests
        {
            tasks
                .entry((arch, profile, out_name))
                .or_default()
                .push(opentmk);
        }

        for ((arch, profile, out_name), outvars) in tasks {
            let out_name = out_name.unwrap_or_else(|| "opentmk".to_string());

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "opentmk".into(),
                out_name: out_name.clone(),
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
            });

            ctx.emit_minor_rust_step("report built opentmk", |ctx| {
                let outvars = outvars.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let (efi, pdb) = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::UefiBin { efi, pdb } => {
                            (efi, pdb)
                        }
                        _ => unreachable!(),
                    };

                    let output = OpentmkOutput { efi, pdb };

                    for var in outvars {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
