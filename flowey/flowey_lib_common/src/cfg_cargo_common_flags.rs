// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized configuration for setting "global" cargo command flags, such as
//! `--locked`, `--verbose`, etc...
//!
//! This node can then be depended on by nodes which do fine-grained ops with
//! cargo (e.g: `cargo build`, `cargo doc`, `cargo test`, etc...) to avoid
//! duping the same flag config all over the place.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct Flags {
    pub locked: bool,
    pub verbose: bool,
    pub no_incremental: bool,
}

flowey_config! {
    /// Config for the cfg_cargo_common_flags node.
    pub struct Config {
        pub locked: Option<bool>,
        pub verbose: Option<ConfigVar<bool>>,
        pub no_incremental: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        GetFlags(WriteVar<Flags>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut get_flags = Vec::new();

        for req in requests {
            match req {
                Request::GetFlags(v) => get_flags.push(v),
            }
        }

        let set_locked = config
            .locked
            .ok_or(anyhow::anyhow!("missing config: locked"))?;
        let set_verbose = config
            .verbose
            .ok_or(anyhow::anyhow!("missing config: verbose"))?;
        let set_no_incremental = config.no_incremental.unwrap_or(false);
        let get_flags = get_flags;

        // -- end of req processing -- //

        if get_flags.is_empty() {
            return Ok(());
        }

        ctx.emit_minor_rust_step("report common cargo flags", |ctx| {
            let get_flags = get_flags.claim(ctx);
            let set_verbose = set_verbose.claim(ctx);

            move |rt| {
                let set_verbose = rt.read(set_verbose);
                for var in get_flags {
                    rt.write(
                        var,
                        &Flags {
                            locked: set_locked,
                            verbose: set_verbose,
                            no_incremental: set_no_incremental,
                        },
                    );
                }
            }
        });

        Ok(())
    }
}
