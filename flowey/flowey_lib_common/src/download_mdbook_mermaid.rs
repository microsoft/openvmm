// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `mdbook-mermaid`

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the download_mdbook_mermaid node.
    pub struct Config {
        /// Version of `mdbook-mermaid` to install
        pub version: Option<String>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get a path to `mdbook-mermaid`
        GetMdbookMermaid(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut get_mdbook_mermaid = Vec::new();

        for req in requests {
            match req {
                Request::GetMdbookMermaid(v) => get_mdbook_mermaid.push(v),
            }
        }

        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let get_mdbook_mermaid = get_mdbook_mermaid;

        // -- end of req processing -- //

        if get_mdbook_mermaid.is_empty() {
            return Ok(());
        }

        crate::download_mdbook::download_mdbook_tool(
            ctx,
            "mdbook-mermaid",
            "badboy",
            "mdbook-mermaid",
            &version,
            get_mdbook_mermaid,
        )
    }
}
