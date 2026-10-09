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
        let (version, get_mdbook_mermaid) = crate::download_mdbook::collect_download_requests(
            config.version,
            requests,
            |Request::GetMdbookMermaid(v)| v,
        )?;

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
