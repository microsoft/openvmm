// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the development-only mshv UEFI host boot application.

use crate::common::CommonArch;
use crate::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
pub struct MshvHostBootOutput {
    #[serde(rename = "mshv_host_boot.efi")]
    pub efi: PathBuf,
    #[serde(rename = "mshv_host_boot.pdb")]
    pub pdb: Option<PathBuf>,
}

impl Artifact for MshvHostBootOutput {}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub profile: CommonProfile,
        pub host_boot: WriteVar<MshvHostBootOutput>,
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
            arch,
            profile,
            host_boot,
        } = request;
        let target = match arch {
            CommonArch::X86_64 => "x86_64-unknown-uefi",
            CommonArch::Aarch64 => "aarch64-unknown-uefi",
        }
        .parse()?;
        let output = ctx.reqv(|output| crate::run_cargo_build::Request {
            crate_name: "mshv_host_boot".into(),
            out_name: "mshv_host_boot".into(),
            crate_type: CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target,
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output,
        });
        ctx.emit_minor_rust_step("report mshv UEFI host boot application", |ctx| {
            let output = output.claim(ctx);
            let host_boot = host_boot.claim(ctx);
            move |rt| {
                let crate::run_cargo_build::CargoBuildOutput::UefiBin { efi, pdb } =
                    rt.read(output)
                else {
                    unreachable!()
                };
                rt.write(
                    host_boot,
                    &MshvHostBootOutput {
                        efi,
                        pdb: Some(pdb),
                    },
                );
            }
        });
        Ok(())
    }
}
