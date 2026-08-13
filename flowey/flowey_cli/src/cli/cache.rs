// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Manage flowey's memoization store.

use anyhow::Context;
use flowey_core::node::memo::MEMO_STORE_DIR_NAME;
use flowey_core::node::memo::MemoStore;
use flowey_core::node::memo::configured_max_store_size;
use flowey_core::node::memo::format_size_bytes;
use flowey_core::node::memo::parse_size_bytes;
use std::path::PathBuf;

/// Inspect and manage flowey's memoization store.
///
/// The store lives inside flowey's persistent dir, and holds the outputs of
/// expensive steps (splitting debug info, extracting archives, downloading
/// release assets, ...) so that subsequent runs can skip redoing that work.
#[derive(clap::Args)]
pub struct Cache {
    /// Persistent storage directory shared across multiple runs.
    #[clap(long, default_value = "./flowey-persist", global = true)]
    persist_dir: PathBuf,

    #[clap(subcommand)]
    command: CacheCommand,
}

#[derive(clap::Subcommand)]
enum CacheCommand {
    /// Summarize the contents of the store.
    Stats {
        /// List every entry, newest-used first.
        #[clap(long)]
        list: bool,
    },
    /// Evict least-recently-used entries until the store fits within a size
    /// cap.
    Gc {
        /// Maximum store size, e.g: `20GiB`.
        ///
        /// Defaults to $FLOWEY_MEMO_MAX_SIZE, or 20GiB.
        #[clap(long)]
        max_size: Option<String>,
    },
    /// Remove every entry from the store.
    Clear,
}

impl Cache {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            persist_dir,
            command,
        } = self;

        let root = persist_dir.join(MEMO_STORE_DIR_NAME);
        let Some(store) = MemoStore::open_existing(&root)? else {
            println!("no memoization store at {}", root.display());
            return Ok(());
        };

        match command {
            CacheCommand::Stats { list } => {
                let stats = store.stats()?;
                println!("store:   {}", stats.root.display());
                println!("entries: {}", stats.entries);
                println!("size:    {}", format_size_bytes(stats.size_bytes));
                println!(
                    "cap:     {}",
                    format_size_bytes(configured_max_store_size()?)
                );

                if list {
                    let mut entries = store.entries()?;
                    entries.sort_by_key(|e| std::cmp::Reverse(e.last_used_unix_secs));
                    println!();
                    for entry in entries {
                        let recipe = entry
                            .meta
                            .as_ref()
                            .and_then(|m| m.key_description.lines().next())
                            .unwrap_or("<unknown>")
                            .trim_start_matches("recipe = ");
                        println!(
                            "{}  {:>10}  {}",
                            entry.hash,
                            format_size_bytes(entry.size_bytes),
                            recipe
                        );
                    }
                }
            }
            CacheCommand::Gc { max_size } => {
                let max_bytes = match max_size {
                    Some(s) => parse_size_bytes(&s).context("while parsing --max-size")?,
                    None => configured_max_store_size()?,
                };
                let summary = store.gc(max_bytes)?;
                println!(
                    "evicted {} entries ({}), {} remaining",
                    summary.evicted_entries,
                    format_size_bytes(summary.evicted_bytes),
                    format_size_bytes(summary.remaining_bytes),
                );
            }
            CacheCommand::Clear => {
                store.clear()?;
                println!("cleared {}", root.display());
            }
        }

        Ok(())
    }
}
