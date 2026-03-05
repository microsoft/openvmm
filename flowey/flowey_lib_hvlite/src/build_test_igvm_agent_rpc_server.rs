// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `test_igvm_agent_rpc_server` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct TestIgvmAgentRpcServerOutput {
    #[serde(rename = "test_igvm_agent_rpc_server.exe")]
    pub exe: PathBuf,
    #[serde(rename = "test_igvm_agent_rpc_server.pdb")]
    pub pdb: PathBuf,
}

impl Artifact for TestIgvmAgentRpcServerOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub test_igvm_agent_rpc_server: WriteVar<TestIgvmAgentRpcServerOutput>,
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
            test_igvm_agent_rpc_server,
        } = request;

        let target_triple = target.as_triple();

        // Only Windows MSVC is supported for test_igvm_agent_rpc_server
        if target_triple.operating_system != target_lexicon::OperatingSystem::Windows
            || target_triple.environment != target_lexicon::Environment::Msvc
        {
            anyhow::bail!(
                "test_igvm_agent_rpc_server only supports Windows MSVC targets, got: {}",
                target_triple
            );
        }

        let env_key = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            target_triple.to_string().replace('-', "_").to_uppercase()
        );

        let mut env: BTreeMap<String, String> = BTreeMap::new();

        // Enable CRT static linking
        env.insert(env_key, "-Ctarget-feature=+crt-static".to_string());

        let extra_env = Some(ReadVar::from_static(env));

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "test_igvm_agent_rpc_server".into(),
            out_name: "test_igvm_agent_rpc_server".into(),
            crate_type: CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target_triple,
            no_split_dbg_info: false,
            extra_env,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built test_igvm_agent_rpc_server", |ctx| {
            let test_igvm_agent_rpc_server = test_igvm_agent_rpc_server.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        TestIgvmAgentRpcServerOutput { exe, pdb }
                    }
                    _ => unreachable!(
                        "unsupported build output variant for test_igvm_agent_rpc_server"
                    ),
                };

                rt.write(test_igvm_agent_rpc_server, &output);
            }
        });

        Ok(())
    }
}
