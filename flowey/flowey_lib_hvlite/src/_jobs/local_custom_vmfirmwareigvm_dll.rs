// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub igvm_payload: PathBuf,
        pub arch: CommonArch,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_vmfirmwareigvm_dll::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            igvm_payload,

            artifact_dir,
            done,
        } = request;

        let built_dll = ctx.reqv(|v| crate::build_vmfirmwareigvm_dll::Request {
            arch,
            igvm: ReadVar::from_static(crate::run_igvmfilegen::IgvmOutput {
                igvm_bin: igvm_payload,
                igvm_map: None,
                igvm_tdx_json: None,
                igvm_snp_json: None,
                igvm_vbs_json: None,
            }),
            // fixed version to signal that this is a custom dll
            dll_version: ReadVar::from_static((1, 0, 1337, 0)),
            internal_dll_name: "vmfirmwareigvm.dll".into(),
            vmfirmwareigvm_dll: v,
        });

        ctx.emit_rust_step("copy resulting vmfirmwareigvm.dll", |ctx| {
            done.claim(ctx);
            let artifact_dir = artifact_dir.claim(ctx);
            let built_dll = built_dll.claim(ctx);
            |rt| {
                let artifact_dir = rt.read(artifact_dir);
                let built_dll = rt.read(built_dll);

                fs_err::copy(built_dll.dll, artifact_dir.join("vmfirmwareigvm.dll"))?;

                for e in fs_err::read_dir(artifact_dir)? {
                    let e = e?;
                    log::info!("{}", e.path().display());
                }
                Ok(())
            }
        });

        Ok(())
    }
}
