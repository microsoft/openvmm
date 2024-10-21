// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// (debug) list all registered patches
#[derive(clap::Args)]
pub struct ListPatches;

impl ListPatches {
    pub fn run(self) -> anyhow::Result<()> {
        let mut v = flowey_core::patch::patchfn_by_modpath()
            .keys()
            .collect::<Vec<_>>();
        v.sort();
        for fn_name in v {
            println!("{fn_name}")
        }

        Ok(())
    }
}
