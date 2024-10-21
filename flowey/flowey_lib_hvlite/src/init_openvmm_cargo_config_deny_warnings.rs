// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tweak `.cargo/config.toml` to deny warnings.

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        DenyWarnings(bool),
        Done(WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut done = Vec::new();
        let mut deny_warnings = None;

        for req in requests {
            match req {
                Request::Done(v) => done.push(v),
                Request::DenyWarnings(v) => {
                    same_across_all_reqs("DenyWarnings", &mut deny_warnings, v)?
                }
            }
        }

        let deny_warnings =
            deny_warnings.ok_or(anyhow::anyhow!("Missing essential request: DenyWarnings"))?;

        // -- end of req processing -- //

        if done.is_empty() {
            return Ok(());
        }

        if !deny_warnings {
            ctx.emit_side_effect_step([], done);
            return Ok(());
        }

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("set '-Dwarnings' in .cargo/config.toml", move |ctx| {
            done.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let path = rt.read(openvmm_repo_path).join(".cargo/config.toml");
                let data = fs_err::read_to_string(&path)?;
                let data = data.replace("### ENABLE_IN_CI", "");
                fs_err::write(path, data)?;
                Ok(())
            }
        });

        Ok(())
    }
}
