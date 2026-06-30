// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds the `ipmi_kcs_ffi` C-ABI static library and bundles it with its C
//! header for cross-language consumption (e.g. the C++ Legacy HCL build in the
//! OS repo).
//!
//! Modeled on [`crate::build_and_test_vmgs_lib`]. Unlike `vmgs_lib`, there is no
//! in-repo C test harness to exercise: the only consumer is the out-of-tree C++
//! Legacy HCL stack, which validates the contract in its own build. We
//! therefore build the staticlib and publish it alongside `ipmi_kcs.h`.

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::run_cargo_build::CargoBuildOutput;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum IpmiKcsFfiOutput {
    LinuxStaticLib {
        #[serde(rename = "libipmi_kcs_ffi.a")]
        a: PathBuf,
        #[serde(rename = "ipmi_kcs.h")]
        header: PathBuf,
    },
    WindowsStaticLib {
        #[serde(rename = "ipmi_kcs_ffi.lib")]
        lib: PathBuf,
        #[serde(rename = "ipmi_kcs.h")]
        header: PathBuf,
    },
}

impl Artifact for IpmiKcsFfiOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub ipmi_kcs_ffi: WriteVar<IpmiKcsFfiOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            ipmi_kcs_ffi,
        } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "ipmi_kcs_ffi".into(),
            out_name: "ipmi_kcs_ffi".into(),
            crate_type: CargoCrateType::StaticLib,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_minor_rust_step("report built ipmi_kcs_ffi", |ctx| {
            let output = output.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let ipmi_kcs_ffi = ipmi_kcs_ffi.claim(ctx);
            move |rt| {
                let openvmm_repo_path = rt.read(openvmm_repo_path);
                let header = openvmm_repo_path.join("vm/devices/ipmi_kcs_ffi/ipmi_kcs.h");

                let built = match rt.read(output) {
                    CargoBuildOutput::LinuxStaticLib { a } => {
                        IpmiKcsFfiOutput::LinuxStaticLib { a, header }
                    }
                    CargoBuildOutput::WindowsStaticLib { lib, pdb: _ } => {
                        IpmiKcsFfiOutput::WindowsStaticLib { lib, header }
                    }
                    _ => unreachable!(),
                };

                rt.write(ipmi_kcs_ffi, &built);
            }
        });

        Ok(())
    }
}
