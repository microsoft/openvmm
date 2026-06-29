// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the `incubator` binary

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct IncubatorOutput {
    #[serde(rename = "incubator")]
    pub bin: PathBuf,
    #[serde(rename = "incubator.dbg")]
    pub dbg: PathBuf,
    /// Directory of incubator profile TOML files, copied from
    /// `petri/incubator/profiles/` in the repo. Carried in the artifact so the
    /// (checkout-less) VMM test runner job can select a profile by name.
    #[serde(rename = "profiles")]
    pub profiles: PathBuf,
}

impl Artifact for IncubatorOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub incubator: WriteVar<IncubatorOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            incubator,
        } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "incubator".into(),
            out_name: "incubator".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built incubator", |ctx| {
            let incubator = incubator.claim(ctx);
            let output = output.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let openvmm_repo_path = rt.read(openvmm_repo_path);
                let profiles = openvmm_repo_path.join("petri/incubator/profiles");
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        IncubatorOutput {
                            bin,
                            dbg: dbg.unwrap(),
                            profiles,
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(incubator, &output);
            }
        });

        Ok(())
    }
}
