// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run OpenVMM CCA tests. Now we run them using emulator, code can be tweaked
//! to support running tests on native hardware platform.
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub build_only: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

const ENV_CCA_TEST_ROOT: &str = "OPENVMM_CCA_TEST_ROOT";
const ENV_CCA_TMK_VMM: &str = "OPENVMM_CCA_TMK_VMM";
const ENV_CCA_SIMPLE_TMK: &str = "OPENVMM_CCA_SIMPLE_TMK";

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tmks::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            test_root,
            build_only,
            done,
        } = request;

        // Generate request to build tmk_vmm
        let tmk_vmm_output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
            target: CommonTriple::Common {
                arch: CommonArch::Aarch64,
                platform: CommonPlatform::LinuxGnu,
            },
            profile: CommonProfile::Debug,
            tmk_vmm: v,
        });

        // Generate request to build simple_tmk
        let simple_tmk_output = ctx.reqv(|v| crate::build_tmks::Request {
            arch: CommonArch::Aarch64,
            profile: CommonProfile::Debug,
            tmks: v,
        });

        ctx.emit_rust_step("running cca tests", |ctx| {
            done.claim(ctx);
            let tmk_vmm_output = tmk_vmm_output.claim(ctx);
            let simple_tmk_output = simple_tmk_output.claim(ctx);
            move |rt| {
                let tmk_vmm_output = rt.read(tmk_vmm_output);
                let crate::build_tmk_vmm::TmkVmmOutput::LinuxBin {
                    bin: tmk_vmm_bin, ..
                } = tmk_vmm_output
                else {
                    anyhow::bail!("expect Linux tmk_vmm only");
                };

                let simple_tmk_output = rt.read(simple_tmk_output);
                let simple_tmk_bin = simple_tmk_output.bin;

                if build_only {
                    log::info!("CCA test artifacts built; skipping Petri test because --build-only was specified");
                    log::info!("tmk_vmm: {}", tmk_vmm_bin.display());
                    log::info!("simple_tmk: {}", simple_tmk_bin.display());
                    return Ok(());
                }

                flowey::shell_cmd!(rt, "cargo test -p vmm_tests --test cca")
                .env(ENV_CCA_TEST_ROOT, &test_root)
                .env(ENV_CCA_TMK_VMM, &tmk_vmm_bin)
                .env(ENV_CCA_SIMPLE_TMK, &simple_tmk_bin)
                .run()
                .with_context(|| "failed to run CCA runtime Petri test")?;

                Ok(())
            }
        });

        Ok(())
    }
}
