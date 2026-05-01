// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
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

fn kib_away_from_zero(bytes: i64) -> i64 {
    if bytes >= 0 {
        (bytes as u64).div_ceil(1024) as i64
    } else {
        -(((-bytes) as u64).div_ceil(1024) as i64)
    }
}

fn verify_sections_size(
    new: &object::File<'_>,
    original: &object::File<'_>,
) -> anyhow::Result<(u64, i64)> {
    println!(
        "{:20} {:>20} {:>20} {:>20}",
        "Section", "Old Size", "New Size", "Difference"
    );

    let mut total_diff_bytes: u64 = 0;
    let mut net_diff_bytes: i64 = 0;
    let mut total_size_bytes: u64 = 0;

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

        let new_bytes = new
            .section_by_name(name.as_str())
            .map(|s| s.size())
            .unwrap_or(0);
        let original_bytes = original
            .section_by_name(name.as_str())
            .map(|s| s.size())
            .unwrap_or(0);
        let diff_bytes = (new_bytes as i64) - (original_bytes as i64);
        total_diff_bytes += diff_bytes.unsigned_abs();
        net_diff_bytes += diff_bytes;
        total_size_bytes += new_bytes;

        // Print any sections that have changed in size
        if new_bytes != original_bytes {
            let new_kib = new_bytes.div_ceil(1024);
            let original_kib = original_bytes.div_ceil(1024);
            let diff_kib = kib_away_from_zero(diff_bytes);
            println!(
                "{name:20} {:>15} KiB {:>15} KiB {:>15} KiB",
                original_kib, new_kib, diff_kib
            );
        }
    }

    println!("Total Size: {} KiB.", total_size_bytes.div_ceil(1024));

    let total_diff_kib = total_diff_bytes.div_ceil(1024);
    let net_diff_kib = kib_away_from_zero(net_diff_bytes);

    Ok((total_diff_kib, net_diff_kib))
}

impl Xtask for VerifySize {
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let original = fs_err::read(&self.original)?;
        let new = fs_err::read(&self.new)?;

        let original_elf = object::File::parse(&*original);
        let new_elf = object::File::parse(&*new);

        println!("Verifying size for {}:", (&self.new.display()));

        let (total_diff, net_diff) = match (original_elf, new_elf) {
            (Ok(orig), Ok(new_parsed)) => verify_sections_size(&new_parsed, &orig)?,
            // Both files fail to parse — assume they are raw binary images
            // (e.g. aarch64 raw kernel Image). If only one fails, that's an
            // error indicating mismatched file formats.
            (Err(orig_err), Err(new_err)) => {
                println!(
                    "(files are not parseable object files, comparing raw file sizes)\n\
                     original parse error: {orig_err}\n\
                     new parse error: {new_err}"
                );
                let orig_bytes = original.len() as u64;
                let new_bytes = new.len() as u64;
                let diff_bytes = (new_bytes as i64) - (orig_bytes as i64);
                let orig_kib = orig_bytes.div_ceil(1024);
                let new_kib = new_bytes.div_ceil(1024);
                let diff_kib = kib_away_from_zero(diff_bytes);
                println!(
                    "{:20} {:>15} {:>15} {:>16}",
                    "Section", "Original", "New", "Difference"
                );
                println!(
                    "{:20} {:>15} {:>15} {:>16}",
                    "raw file",
                    format!("{orig_kib} KiB ({orig_bytes} B)"),
                    format!("{new_kib} KiB ({new_bytes} B)"),
                    format!("{diff_kib} KiB ({diff_bytes:+} B)")
                );
                println!("Total Size: {new_kib} KiB ({new_bytes} bytes).");
                // Compare in bytes for accuracy, convert to KiB for the
                // threshold check below.
                let total_diff_kib = diff_bytes.unsigned_abs().div_ceil(1024);
                (total_diff_kib, diff_kib)
            }
            (Err(e), Ok(_)) => {
                anyhow::bail!(
                    "failed to parse original binary '{}': {e}",
                    self.original.display()
                );
            }
            (Ok(_), Err(e)) => {
                anyhow::bail!("failed to parse new binary '{}': {e}", self.new.display());
            }
        };

        println!("Net difference: {net_diff} KiB.");
        println!("Total difference: {total_diff} KiB.");

        const ALLOWED: u64 = 50;
        if total_diff > ALLOWED {
            anyhow::bail!(
                "{} size verification failed: \
            The total difference ({} KiB) is greater than the allowed difference ({} KiB).",
                self.new.display(),
                total_diff,
                ALLOWED
            );
        }

        Ok(())
    }
}
