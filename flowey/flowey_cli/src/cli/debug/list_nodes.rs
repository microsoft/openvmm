// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// (debug) list all registered flowey nodes
#[derive(clap::Args)]
pub struct ListNodes;

impl ListNodes {
    pub fn run(self) -> anyhow::Result<()> {
        // FUTURE: it might be worth improving this output to leverage the DAG?
        let mut v = flowey_core::node::list_all_registered_nodes()
            .map(|h| h.modpath())
            .collect::<Vec<_>>();
        v.sort();
        for node in v {
            println!("{node}")
        }

        Ok(())
    }
}
