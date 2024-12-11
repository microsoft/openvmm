// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use anyhow::Context;
use object::read::Object;
use object::read::ObjectSection;
use object::read::ObjectSymbol;
use object::read::ObjectSymbolTable;
use std::collections::HashMap;

struct ExpectedSymbol<'a> {
    name: &'a str,
    start_symbol: &'a str,
    end_symbol: &'a str,
    size: u64,
}

impl<'a> ExpectedSymbol<'a> {
    const fn new(name: &'a str, start_symbol: &'a str, end_symbol: &'a str, size: u64) -> Self {
        Self {
            name,
            start_symbol,
            end_symbol,
            size,
        }
    }
}

struct ExpectedSection<'a> {
    name: &'a str,
    size: u64,
}

impl<'a> ExpectedSection<'a> {
    const fn new(name: &'a str, size: u64) -> Self {
        Self { name, size }
    }
}

// Known target sizes

const HCL_KERNEL_SHIP_NAME: &str = "hcl-kernel-ship";
const HCL_KERNEL_SHIP_TOLERANCE: u64 = 100; //KiB
#[rustfmt::skip]
const HCL_KERNEL_SHIP_SIZES: [ExpectedSymbol<'_>; 5] = [
    ExpectedSymbol::new("Text",         "_stext",               "_etext",               4038),
    ExpectedSymbol::new("RO Data",      "__start_rodata",       "__end_rodata",         1096),
    ExpectedSymbol::new("RW Data",      "_sdata",               "_edata",               463),
    ExpectedSymbol::new("BSS",          "__bss_start",          "__bss_stop",           572),
    ExpectedSymbol::new("Entry Pad",    "__kprobes_text_end",   "__entry_text_start",   1),
    // "Entry Pad" is the padding from PTI.
    // It is the amount of space remaining before the PTI
    // alignment jumps to the next 2MiB boundary.
    // Once it goes below zero, another 2MiB of binary space will be consumed.
];

// project-target-profile
const UNDERHILL_MUSL_RELEASE_NAME: &str = "underhill-x86_64-unknown-linux-musl-release";
const UNDERHILL_MUSL_RELEASE_TOLERANCE: u64 = 100; //KiB
#[rustfmt::skip]
const UNDERHILL_MUSL_RELEASE_SIZES: [ExpectedSection<'_>; 10] = [
    // Only sections greater than 1 KiB are included.
    ExpectedSection::new(".rela.dyn",           1739),
    ExpectedSection::new(".text",               11067),
    ExpectedSection::new(".rodata",             1068),
    ExpectedSection::new(".eh_frame_hdr",       137),
    ExpectedSection::new(".gcc_except_table",   8),
    ExpectedSection::new(".eh_frame",           715),
    ExpectedSection::new(".data.rel.ro",        1121),
    ExpectedSection::new(".got",                2),
    ExpectedSection::new(".data",               71),
    ExpectedSection::new(".bss",                237),
];

/// Xtask to track changes to binary sizes we care about.
#[derive(Debug, clap::Parser)]
#[clap(about = "Verify the size of a binary hasn't changed more than allowed.")]
pub struct VerifySize {
    /// Target binary path
    #[clap(short, long, required(true))]
    path: std::path::PathBuf,

    /// The target type of the binary
    #[clap(short, long, required(true), value_parser = clap::builder::PossibleValuesParser::new([UNDERHILL_MUSL_RELEASE_NAME, HCL_KERNEL_SHIP_NAME]))]
    target: String,
}

fn verify_sections_size(
    elf: &object::File<'_>,
    expected_sections: &[ExpectedSection<'_>],
) -> anyhow::Result<u64> {
    println!(
        "{:20} {:>15} {:>15} {:>16}",
        "Section", "New Size (KiB)", "Old Size (KiB)", "Difference (KiB)"
    );

    let mut total_diff: u64 = 0;
    let mut total_size: u64 = 0;
    for ExpectedSection { name, size } in expected_sections {
        // If the section size is `0` it typically won't be reported in the file at all.
        let section = if *size == 0 {
            elf.section_by_name(name).map(|s| s.size()).unwrap_or(0)
        } else {
            elf.section_by_name(name)
                .context(format!("Unable to find section \"{}\"", name))?
                .size()
        };

        let actual_size = (section / 1024) as i64;
        let diff = actual_size - (*size as i64);
        total_size += actual_size as u64;
        total_diff += diff.unsigned_abs();
        println!("{name:20} {actual_size:15} {size:15} {diff:16}");
    }

    println!("Total Size: {total_size} KiB.");

    Ok(total_diff)
}

fn verify_symbols_size(
    elf: &object::File<'_>,
    expected_symbols: &[ExpectedSymbol<'_>],
) -> anyhow::Result<u64> {
    let symbol_table = elf.symbol_table().context("No symbol table found")?;

    let symbols: HashMap<_, _> = symbol_table
        .symbols()
        .filter(|s| s.name().is_ok())
        .map(|s| (s.name().unwrap(), s.address()))
        .collect();

    println!(
        "{:10} {:>20} {:>20} {:>15} {:>15} {:>16}",
        "Area",
        "Start Address",
        "End Address",
        "New Size (KiB)",
        "Old Size (KiB)",
        "Difference (KiB)"
    );

    let mut total_diff: u64 = 0;
    for ExpectedSymbol {
        name,
        start_symbol,
        end_symbol,
        size,
    } in expected_symbols
    {
        let start_addr = symbols
            .get(start_symbol)
            .with_context(|| format!("Unable to find symbol: \"{}\"", start_symbol))?;
        let end_addr = symbols
            .get(end_symbol)
            .with_context(|| format!("Unable to find symbol: \"{}\"", end_symbol))?;
        let actual_size = ((end_addr - start_addr) / 1024) as i64;
        let diff = actual_size - (*size as i64);
        total_diff += diff.unsigned_abs();
        println!(
            "{name:10} {start_addr:#20X} {end_addr:#20X} {actual_size:15} {size:15} {diff:16}"
        );
    }

    Ok(total_diff)
}

impl Xtask for VerifySize {
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let data = fs_err::read(&self.path)?;

        let elf = object::File::parse(&*data).or_else(|e| {
            anyhow::bail!(
                r#"Unable to parse target file "{}". Error: "{}""#,
                &self.path.display(),
                e
            )
        })?;

        let target = self.target.as_str();

        println!("Verifying size for {}:", target);
        let (total_diff, tolerance) = match target {
            HCL_KERNEL_SHIP_NAME => (
                verify_symbols_size(&elf, &HCL_KERNEL_SHIP_SIZES)?,
                HCL_KERNEL_SHIP_TOLERANCE,
            ),
            UNDERHILL_MUSL_RELEASE_NAME => (
                verify_sections_size(&elf, &UNDERHILL_MUSL_RELEASE_SIZES)?,
                UNDERHILL_MUSL_RELEASE_TOLERANCE,
            ),
            other => anyhow::bail!("Invalid target: \"{}\"", other),
        };

        println!("Total difference: {total_diff} KiB.");

        if total_diff > tolerance {
            anyhow::bail!("{} size verification failed: The total difference ({} KiB) is greater than the allowed difference ({} KiB).", target, total_diff, tolerance);
        }

        Ok(())
    }
}
