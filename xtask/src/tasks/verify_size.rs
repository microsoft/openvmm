// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::Xtask;
use object::read::Object;
use object::read::ObjectSection;
use std::collections::HashMap;

/// Runs a size comparison and outputs a diff of two given binaries
#[derive(Debug, clap::Parser)]
#[clap(about = "Verify the size of a binary hasn't changed more than allowed.")]
pub struct VerifySize {
    /// Old binary path
    #[clap(short, long, required(true))]
    original: std::path::PathBuf,

    /// New binary path
    #[clap(short, long, required(true))]
    new: std::path::PathBuf,
}

fn verify_sections_size(
    new: &object::File<'_>,
    original: &object::File<'_>,
) -> anyhow::Result<u64> {
    println!(
        "{:20} {:>15} {:>15} {:>16}",
        "Section", "New Size (KiB)", "Old Size (KiB)", "Difference (KiB)"
    );

    let mut total_diff: u64 = 0;
    let mut total_size: u64 = 0;

    let expected_sections: HashMap<_, _> = original
        .sections()
        .filter_map(|s| {
            s.name()
                .ok()
                .map(|name| (name.to_string(), (s.size() as i64) / 1024))
        })
        .filter(|(_, size)| *size > 0)
        .collect();

    for section in new.sections() {
        let name = section.name().unwrap();
        let size = (section.size() / 1024) as i64;
        let expected_size = *expected_sections.get(name).unwrap_or(&0);
        let diff = (size as i64) - (expected_size as i64);
        total_diff += diff.unsigned_abs();
        total_size += size as u64;

        // Print any non-zero sections in the newer binary and any sections that differ in size from the original.
        if size != 0 || diff != 0 {
            println!("{name:20} {size:15} {expected_size:15} {diff:16}");
        }
    }

    println!("Total Size: {total_size} KiB.");

    Ok(total_diff)
}

impl Xtask for VerifySize {
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let original = fs_err::read(&self.original)?;
        let new = fs_err::read(&self.new)?;

        let original_elf = object::File::parse(&*original).or_else(|e| {
            anyhow::bail!(
                r#"Unable to parse target file "{}". Error: "{}""#,
                &self.original.display(),
                e
            )
        })?;

        let new_elf = object::File::parse(&*new).or_else(|e| {
            anyhow::bail!(
                r#"Unable to parse target file "{}". Error: "{}""#,
                &self.new.display(),
                e
            )
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
