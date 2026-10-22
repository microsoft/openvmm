// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the x86_64 SNP boot shim.

use crate::common::CommonArch;
use crate::run_cargo_build::BuildProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
pub struct SnpBootshimOutput {
    pub bin: PathBuf,
}

flowey_request! {
    pub struct Request {
        pub snp_bootshim: WriteVar<SnpBootshimOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let target = target_lexicon::Triple {
            architecture: CommonArch::X86_64.as_arch(),
            operating_system: target_lexicon::OperatingSystem::None_,
            environment: target_lexicon::Environment::Unknown,
            vendor: target_lexicon::Vendor::Custom(target_lexicon::CustomVendor::Static(
                "minimal_rt",
            )),
            binary_format: target_lexicon::BinaryFormat::Unknown,
        };

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "snp_bootshim".into(),
            out_name: "snp_bootshim".into(),
            crate_type: CargoCrateType::Bin,
            profile: BuildProfile::BootDev,
            features: Default::default(),
            target,
            no_split_dbg_info: true,
            extra_env: Some(ReadVar::from_static(
                [("RUSTC_BOOTSTRAP".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
            )),
            pre_build_deps: Vec::new(),
            output: v,
        });

        output.write_into_with(ctx, request.snp_bootshim, |o| match o {
            crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, .. } => {
                SnpBootshimOutput { bin }
            }
            _ => unreachable!("snp_bootshim builds for minimal_rt"),
        });
        Ok(())
    }
}
