// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::Xtask;
use anyhow::Context;
use object::read::Object;
use object::read::ObjectSection;
use std::collections::HashSet;

/// Runs a size comparison and outputs a diff of two given binaries
#[derive(Debug, clap::Parser)]
#[clap(about = "Verify the size of a binary hasn't changed more than allowed.")]
pub struct VerifySize {
    /// Old binary path
    #[clap(short, long)]
    original: std::path::PathBuf,

    /// New binary path
    #[clap(short, long)]
    new: std::path::PathBuf,
}

fn verify_sections_size(
    new: &object::File<'_>,
    original: &object::File<'_>,
) -> anyhow::Result<u64> {
    println!(
        "{:20} {:>15} {:>15} {:>16}",
        "Section", "Old Size (KiB)", "New Size (KiB)", "Difference (KiB)"
    );

    let mut total_diff: u64 = 0;
    let mut total_size: u64 = 0;

    let all_original_sections: Vec<_> = original
        .sections()
        .filter_map(|s| s.name().ok().map(|name| name.to_string()))
        .collect();
    let all_new_sections: Vec<_> = new
        .sections()
        .filter_map(|s| s.name().ok().map(|name| name.to_string()))
        .collect();

    let all_sections: HashSet<_> = all_original_sections
        .into_iter()
        .chain(all_new_sections)
        .collect();

    for section in all_sections {
        let name = section;

        let new_size = new
            .section_by_name(name.as_str())
            .map(|s| s.size() / 1024)
            .unwrap_or(0);
        let original_size = original
            .section_by_name(name.as_str())
            .map(|s| s.size() / 1024)
            .unwrap_or(0);
        let diff = (new_size as i64) - (original_size as i64);
        total_diff += diff.unsigned_abs();
        total_size += new_size;

        // Print any sections that have changed in size
        if new_size != original_size {
            println!("{name:20} {original_size:15} {new_size:15} {diff:16}");
        }
    }

    println!("Total Size: {total_size} KiB.");

    Ok(total_diff)
}

impl Xtask for VerifySize {
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let original = fs_err::read(&self.original)?;
        let new = fs_err::read(&self.new)?;

        let original_elf = object::File::parse(&*original).with_context(|| {
            format!(
                r#"Unable to parse target file "{}"."#,
                &self.original.display()
            )
        })?;

        let new_elf = object::File::parse(&*new).with_context(|| {
            format!(r#"Unable to parse target file "{}"."#, &self.new.display(),)
        })?;

        println!("Verifying size for {}:", (&self.new.display()));
        let total_diff = verify_sections_size(&new_elf, &original_elf)?;

        println!("Total difference: {total_diff} KiB.");

        if total_diff > 100 {
            anyhow::bail!("{} size verification failed: The total difference ({} KiB) is greater than the allowed difference ({} KiB).", self.new.display(), total_diff, 100);
        }

        Ok(())
    }
}
