// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build an instance of `vmfirmwareigvm.dll`

use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonTriple;
use crate::run_igvmfilegen::IgvmOutput;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct VmfirmwareigvmDllOutput {
    pub dll: PathBuf,
}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub igvm: ReadVar<IgvmOutput>,
        /// (major, minor, patch, revision)
        pub dll_version: ReadVar<(u16, u16, u16, u16)>,
        pub internal_dll_name: String,
        pub vmfirmwareigvm_dll: WriteVar<VmfirmwareigvmDllOutput>,
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
            igvm,
            internal_dll_name,
            dll_version,
            vmfirmwareigvm_dll,
        } = request;

        let extra_env = ctx.emit_rust_stepv("determine vmfirmwareigvm_dll env vars", |ctx| {
            let igvm = igvm.claim(ctx);
            let dll_version = dll_version.claim(ctx);
            move |rt| {
                let mut extra_env = BTreeMap::new();

                // set the various build-time env vars `vmfirmwareigvm_dll` expects
                {
                    extra_env.insert("UH_DLL_NAME".into(), internal_dll_name);
                    extra_env.insert(
                        "UH_IGVM_PATH".into(),
                        // rc.exe treats '\' in windows paths as escape sequences.
                        // there is likely a more robust solution to fix this, but
                        // a simple swap from '\' to '/' seems to work fine for now
                        rt.read(igvm)
                            .igvm_bin
                            .absolute()
                            .context("Failed to make igvm bin path absolute")?
                            .display()
                            .to_string()
                            .replace('\\', "/"),
                    );
                    let (major, minor, patch, revision) = rt.read(dll_version);
                    extra_env.insert("UH_MAJOR".into(), major.to_string());
                    extra_env.insert("UH_MINOR".into(), minor.to_string());
                    extra_env.insert("UH_PATCH".into(), patch.to_string());
                    extra_env.insert("UH_REVISION".into(), revision.to_string());
                }

                // workaround for the fact that hvlite's root-level `.cargo/config.toml`
                // currently sets a bunch of extraneous linker flags via
                //
                // [target.'cfg(all(windows, target_env = "msvc"))']
                extra_env.insert("RUSTFLAGS".into(), "".into());

                Ok(extra_env)
            }
        });

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "vmfirmwareigvm_dll".into(),
            out_name: "vmfirmwareigvm_dll".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::DynamicLib,
            profile: crate::run_cargo_build::BuildProfile::Release,
            features: Default::default(),
            target: CommonTriple::Common {
                arch,
                platform: crate::run_cargo_build::common::CommonPlatform::WindowsMsvc,
            }
            .as_triple(),
            no_split_dbg_info: false,
            extra_env: Some(extra_env),
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_rust_step("report built vmfirmwareigvm_dll", |ctx| {
            let vmfirmwareigvm_dll = vmfirmwareigvm_dll.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsDynamicLib {
                        dll,
                        // this is a resource dll, so these don't matter
                        dll_lib: _,
                        pdb: _,
                    } => VmfirmwareigvmDllOutput { dll },
                    _ => unreachable!(),
                };

                rt.write(vmfirmwareigvm_dll, &output);

                Ok(())
            }
        });

        Ok(())
    }
}
