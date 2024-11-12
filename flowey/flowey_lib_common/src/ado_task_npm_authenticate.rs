// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `npmAuthenticate@0`

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Register a `.npmrc` file which includes authentication info
        UsingNpmrc(ReadVar<PathBuf>),
        /// Ensure authentication has been performed
        Done(WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut npmrcs = Vec::new();
        let mut done = Vec::new();

        for req in requests {
            match req {
                Request::UsingNpmrc(v) => npmrcs.push(v),
                Request::Done(v) => done.push(v),
            }
        }

        let did_run = npmrcs
            .into_iter()
            .map(|npmrc| {
                let npmrc = npmrc.map(ctx, |x| x.display().to_string());
                let (did_run, claim_did_run) = ctx.new_var();
                ctx.emit_ado_step("Authenticate npm", move |ctx| {
                    claim_did_run.claim(ctx);
                    let npmrc = npmrc.claim(ctx);
                    move |rt| {
                        let npmrc = rt.get_var(npmrc);
                        let npmrc = npmrc.as_raw_var_name();

                        format!(
                            r#"
                                    - task: npmAuthenticate@0
                                      inputs:
                                        workingFile: $({npmrc})
                                "#
                        )
                    }
                });
                did_run
            })
            .collect::<Vec<_>>();

        ctx.emit_side_effect_step(did_run, done);

        Ok(())
    }
}
