// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Update components in CCA emulation environment according to options specified.
use flowey::node::prelude::*;

#[derive(Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubCmds {
    pub rebuild: bool,
    pub tfa_rev: Option<String>,
    pub tfrmm_rev: Option<String>,
    pub plane0_linux_rev: Option<String>,
}

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub sub_cmds: SubCmds,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            test_root: _,
            sub_cmds,
            done,
        } = request;

        let SubCmds {
            rebuild: _,
            tfa_rev: _,
            tfrmm_rev: _,
            plane0_linux_rev: _,
        } = sub_cmds;

        ctx.emit_rust_step("update cca emulation environment", |ctx| {
            done.claim(ctx);
            move |_rt| {
                Ok(())
            }
        });

        Ok(())
    }
}
