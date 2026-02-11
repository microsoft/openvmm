// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check if shell must be relaunched to refresh environment variables

use flowey::node::prelude::*;

new_simple_flow_node!(struct Node);

#[derive(Serialize, Deserialize)]
pub enum BinOrEnv {
    Bin(String),
    Env(String, String),
}

flowey_request! {
    pub struct Params {
        /// Ensure requested binary is available on path or environment variable contains expected value
        pub check: ReadVar<Option<BinOrEnv>>,
        pub done: Vec<WriteVar<SideEffect>>,
    }
}

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_dep: &mut ImportCtx<'_>) {
        // no deps
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("only supported on the local backend at this time");
        }

        let Params { check, done } = request;

        // -- end of req processing -- //

        if done.is_empty() {
            return Ok(());
        }

        let check_install = {
            move |_: &mut RustRuntimeServices<'_>, bin: &String| {
                if which::which(bin).is_err() {
                    anyhow::bail!(format!("did not find {} on $PATH", bin));
                }

                anyhow::Ok(())
            }
        };

        let check_env = {
            move |rt: &mut RustRuntimeServices<'_>, env: &String, expected: &String| {
                let env = rt.sh.var(env)?;

                if !env.contains(expected) {
                    anyhow::bail!(format!("did not find '{}' in {}", expected, env));
                }

                anyhow::Ok(())
            }
        };

        ctx.emit_rust_step("ensure binaries are available on path", move |ctx| {
            done.claim(ctx);
            let check = check.claim(ctx);

            move |rt| {
                let check = rt.read(check);
                if check.is_none() {
                    return Ok(());
                }

                let check = check.unwrap();
                if match check {
                    BinOrEnv::Bin(bin) => {
                        check_install(rt, &bin)
                    }
                    BinOrEnv::Env(env, expected) => {
                        check_env(rt, &env, &expected)
                    }
                }.is_err() {
                    let args = std::env::args().collect::<Vec<_>>().join(" ");
                    anyhow::bail!("To ensure installed dependencies are available on your $PATH, please restart your shell, and re-run: `{args}`");
                }
                Ok(())
            }
        });

        Ok(())
    }
}
