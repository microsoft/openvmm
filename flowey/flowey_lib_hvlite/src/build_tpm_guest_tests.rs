// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `tpm_guest_tests` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum TpmGuestTestsOutput {
    WindowsBin {
        #[serde(rename = "tpm_guest_tests.exe")]
        exe: PathBuf,
        #[serde(rename = "tpm_guest_tests.pdb")]
        pdb: PathBuf,
    },
    LinuxBin {
        #[serde(rename = "tpm_guest_tests")]
        bin: PathBuf,
        #[serde(rename = "tpm_guest_tests.dbg")]
        dbg: PathBuf,
    },
}

impl Artifact for TpmGuestTestsOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub tpm_guest_tests: WriteVar<TpmGuestTestsOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            tpm_guest_tests,
        } = request;

        let target_triple = target.as_triple();
        let extra_env = if target_triple.environment == target_lexicon::Environment::Msvc {
            let env_key = format!(
                "CARGO_TARGET_{}_RUSTFLAGS",
                target_triple.to_string().replace('-', "_").to_uppercase()
            );

            let mut env: BTreeMap<String, String> = BTreeMap::new();

            // Enable CRT static linking
            env.insert(env_key, "-Ctarget-feature=+crt-static".to_string());

            Some(ReadVar::from_static(env))
        } else {
            None
        };

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "tpm_guest_tests".into(),
            out_name: "tpm_guest_tests".into(),
            crate_type: CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target_triple,
            no_split_dbg_info: false,
            extra_env,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built tpm_guest_tests", |ctx| {
            let tpm_guest_tests = tpm_guest_tests.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        TpmGuestTestsOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        let dbg = dbg.unwrap_or_else(|| {
                            let mut candidate = bin.clone();
                            candidate.set_extension("dbg");
                            candidate
                        });
                        TpmGuestTestsOutput::LinuxBin { bin, dbg }
                    }
                    _ => unreachable!("unsupported build output variant for tpm_guest_tests"),
                };

                rt.write(tpm_guest_tests, &output);
            }
        });

        Ok(())
    }
}
