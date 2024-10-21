// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use clap::Parser;
use toml_edit::Table;

#[derive(Parser)]
#[clap(about = "Ensure all crates have properly workspaced lint configuration")]
pub struct VerifyLints {
    /// Add configuration when it is missing.
    #[clap(long)]
    pub fix: bool,
}

impl Xtask for VerifyLints {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        // Find Cargo.tomls.
        let files = ignore::Walk::new(ctx.root)
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    if entry.file_name() == "Cargo.toml" {
                        Some(entry.into_path())
                    } else {
                        None
                    }
                }
                Err(err) => {
                    log::error!("error when walking over subdirectories: {}", err);
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut success = true;

        for f in files {
            let contents = fs_err::read_to_string(&f)?;
            let mut parsed = contents.parse::<toml_edit::Document>()?;

            if parsed.get("workspace").is_some() {
                // workspace root, skip
                continue;
            }

            let lints_item = parsed.as_table().get("lints");
            let lints_table = lints_item.and_then(|l| l.as_table());
            let valid = lints_table
                .map(|t| t.len() == 1 && t.get("workspace").and_then(|w| w.as_bool()) == Some(true))
                .unwrap_or(false);

            if !valid {
                if self.fix {
                    parsed.remove("lints");
                    parsed.insert("lints", {
                        let mut tab = Table::new();
                        tab.insert("workspace", toml_edit::value(true));
                        toml_edit::Item::Table(tab)
                    });
                    fs_err::write(&f, parsed.to_string())?;
                    log::info!("fixed lints section in {}", f.display());
                } else {
                    log::error!("invalid lints section in {}", f.display());
                    success = false;
                }
            }
        }

        if !success {
            anyhow::bail!("lints section is invalid in some Cargo.toml files");
        }
        Ok(())
    }
}
