// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;

pub fn check_copyright(path: &Path, fix: bool) -> anyhow::Result<()> {
    const HEADER_MIT_FIRST: &str = "Copyright (c) Microsoft Corporation.";
    const HEADER_MIT_SECOND: &str = "Licensed under the MIT License.";

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(
        ext,
        "rs" | "c" | "proto" | "toml" | "ts" | "js" | "py" | "ps1" | "config"
    ) {
        return Ok(());
    }

    let f = BufReader::new(File::open(path)?);
    let mut lines = f.lines();
    let (script_interpreter_line, blank_after_script_interpreter_line, first_content_line) = {
        let line = lines.next().unwrap_or(Ok(String::new()))?;
        // Besides the "py", "ps1, "toml", and "config" files, only for Rust,
        // `#!` is in the first set of the grammar. That's why we need to check
        // the extension for not being "rs".
        // Someone may decide to put a script interpreter line (aka "shebang")
        // in a .config or a .toml file, and mark the file as executable. While
        // that's not common, we choose not to constrain creativity.
        if line.starts_with("#!") && ext != "rs" {
            let script_interpreter_line = line;
            let after_script_interpreter_line = lines.next().unwrap_or(Ok(String::new()))?;
            (
                Some(script_interpreter_line),
                Some(after_script_interpreter_line.is_empty()),
                lines.next().unwrap_or(Ok(String::new()))?,
            )
        } else {
            (None, None, line)
        }
    };
    let second_content_line = lines.next().unwrap_or(Ok(String::new()))?;
    let third_content_line = lines.next().unwrap_or(Ok(String::new()))?;

    // Preserve any files which are copyright, but not by Microsoft.
    if first_content_line.contains("Copyright") && !first_content_line.contains("Microsoft") {
        return Ok(());
    }

    let mut missing_banner = !first_content_line.contains(HEADER_MIT_FIRST)
        || !second_content_line.contains(HEADER_MIT_SECOND);
    let mut missing_blank_line = !third_content_line.is_empty();

    // TEMP: until we have more robust infrastructure for distinct
    // microsoft-internal checks, include this "escape hatch" for preserving
    // non-MIT licensed files when running `xtask fmt` in the msft internal
    // repo. This uses a job-specific env var, instead of being properly plumbed
    // through via `clap`, to make it easier to remove in the future.
    let is_msft_internal = std::env::var("XTASK_FMT_COPYRIGHT_ALLOW_MISSING_MIT").is_ok();
    if is_msft_internal {
        // support both new and existing copyright banner styles
        missing_banner =
            !(first_content_line.contains("Copyright") && first_content_line.contains("Microsoft"));
        missing_blank_line = !second_content_line.is_empty();
    }

    if fix {
        // windows gets touchy if you try and rename files while there are open
        // file handles
        drop(lines);

        if missing_banner || missing_blank_line {
            let path_fix = &{
                let mut p = path.to_path_buf();
                let ok = p.set_extension(format!("{}.fix", ext));
                assert!(ok);
                p
            };

            let mut f = BufReader::new(File::open(path)?);
            let mut f_fixed = File::create(path_fix)?;

            if let Some(script_interpreter_line) = script_interpreter_line {
                writeln!(f_fixed, "{script_interpreter_line}")?;
            }
            if let Some(blank_after_script_interpreter_line) = blank_after_script_interpreter_line {
                if !blank_after_script_interpreter_line {
                    writeln!(f_fixed)?;
                }
            }

            if missing_banner {
                let prefix = match ext {
                    "rs" | "c" | "proto" | "ts" | "js" => "//",
                    "toml" | "py" | "ps1" | "config" => "#",
                    _ => unreachable!(),
                };

                writeln!(f_fixed, "{} {}", prefix, HEADER_MIT_FIRST)?;
                writeln!(f_fixed, "{} {}", prefix, HEADER_MIT_SECOND)?;

                writeln!(f_fixed)?; // also add that missing blank line
            } else if missing_blank_line {
                // copy the valid header from the current file
                let header_lines = if is_msft_internal { 1 } else { 2 };
                for _ in 0..header_lines {
                    let mut s = String::new();
                    f.read_line(&mut s)?;
                    write!(f_fixed, "{}", s)?;
                }

                // ...but then tack on the blank newline as well
                writeln!(f_fixed)?;
            }

            // copy over the rest of the file contents
            std::io::copy(&mut f, &mut f_fixed)?;

            // windows gets touchy if you try and rename files while there are open
            // file handles
            drop(f);
            drop(f_fixed);

            // ...and then swap the file with the newly fixed file
            fs_err::rename(path_fix, path)?;
        }
    }

    // Consider using an enum if there more than three,
    // or the errors need to be compared.
    let mut missing = vec![];
    if missing_banner {
        missing.push("the copyright & license header");
    }
    if missing_blank_line {
        missing.push("a blank line after the copyright & license header");
    }
    if let Some(blank_after_script_interpreter_line) = blank_after_script_interpreter_line {
        if !blank_after_script_interpreter_line {
            missing.push("a blank line after the script interpreter line");
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    if fix {
        log::info!(
            "applied fixes for missing {:?} in {}",
            missing,
            path.display()
        );
        Ok(())
    } else {
        Err(anyhow!("missing {:?} in {}", missing, path.display()))
    }
}
