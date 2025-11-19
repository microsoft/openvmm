// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;

fn commit(source: File, target: &Path) -> std::io::Result<()> {
    source.set_permissions(target.metadata()?.permissions())?;
    let (file, path) = source.into_parts();
    drop(file); // Windows requires the source be closed in some cases.
    fs_err::rename(path, target)
}

pub fn check_copyright(path: &Path, fix: bool) -> anyhow::Result<()> {
    const HEADER_MIT_FIRST: &str = "Copyright (c) Microsoft Corporation.";
    const HEADER_MIT_SECOND: &str = "Licensed under the MIT License.";

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(
        ext,
        "rs" | "c" | "proto" | "toml" | "ts" | "tsx" | "js" | "css" | "html" | "py" | "ps1"
    ) {
        return Ok(());
    }

    let f = BufReader::new(File::open(path)?);
    let mut lines = f.lines();
    let (
        allowed_non_copyright_first_line,
        blank_after_allowed_non_copyright_first_line,
        first_content_line,
    ) = {
        let line = lines.next().unwrap_or(Ok(String::new()))?;
        // Someone may decide to put a script interpreter line (aka "shebang")
        // in a .config or a .toml file, and mark the file as executable. While
        // that's not common, we choose not to constrain creativity.
        //
        // The shebang (`#!`) is part of the valid grammar of Rust, and does not
        // indicate that the file should be interpreted as a script. So we don't
        // allow that line in Rust files.
        //
        // Some HTML files may start with a `<!DOCTYPE html>` line, so let that line pass as well
        if (line.starts_with("#!") && ext != "rs")
            || (line.starts_with("<!DOCTYPE html>") && ext == "html")
        {
            let allowed_non_copyright_first_line = line;
            let after_allowed_non_copyright_first_line =
                lines.next().unwrap_or(Ok(String::new()))?;
            (
                Some(allowed_non_copyright_first_line),
                Some(after_allowed_non_copyright_first_line.is_empty()),
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
    let mut header_lines = 2;

    // TEMP: until we have more robust infrastructure for distinct
    // microsoft-internal checks, include this "escape hatch" for preserving
    // non-MIT licensed files when running `xtask fmt` in the msft internal
    // repo. This uses a job-specific env var, instead of being properly plumbed
    // through via `clap`, to make it easier to remove in the future.
    let is_msft_internal = std::env::var("XTASK_FMT_COPYRIGHT_ALLOW_MISSING_MIT").is_ok();
    if is_msft_internal && missing_banner {
        // support both new and existing copyright banner styles
        missing_banner =
            !(first_content_line.contains("Copyright") && first_content_line.contains("Microsoft"));
        missing_blank_line = !second_content_line.is_empty();
        header_lines = 1;
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

            if let Some(allowed_non_copyright_first_line) = &allowed_non_copyright_first_line {
                writeln!(f_fixed, "{allowed_non_copyright_first_line}")?;
                f.read_line(&mut String::new())?;
            }
            if let Some(blank_after_allowed_non_copyright_first_line) =
                blank_after_allowed_non_copyright_first_line
            {
                if !blank_after_allowed_non_copyright_first_line {
                    writeln!(f_fixed)?;
                }
            }

            if missing_banner {
                let prefix = match ext {
                    "rs" | "c" | "proto" | "ts" | "tsx" | "js" => "//",
                    "toml" | "py" | "ps1" | "config" => "#",
                    "css" => "/*",
                    "html" => "<!--",
                    _ => unreachable!(),
                };

                // Put a space here (if required), so that header lines without a prefix
                // don't end with a trailing space. E.g. ` -->` instead of `-->`.
                let suffix = match ext {
                    "rs" | "c" | "proto" | "ts" | "tsx" | "js" | "toml" | "py" | "ps1"
                    | "config" => "",
                    "css" => " */",
                    "html" => " -->",
                    _ => unreachable!(),
                };

                // Preserve the UTF-8 BOM if it exists.
                if allowed_non_copyright_first_line.is_none()
                    && first_content_line.starts_with('\u{feff}')
                {
                    write!(f_fixed, "\u{feff}")?;
                    // Skip the BOM.
                    f.read_exact(&mut [0; 3])?;
                }

                writeln!(f_fixed, "{} {}{}", prefix, HEADER_MIT_FIRST, suffix)?;
                if !is_msft_internal {
                    writeln!(f_fixed, "{} {}{}", prefix, HEADER_MIT_SECOND, suffix)?;
                }

                writeln!(f_fixed)?; // also add that missing blank line
            } else if missing_blank_line {
                // copy the valid header from the current file
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

            // Windows gets touchy if you try and rename files while there are open
            // file handles.
            drop(f);
            commit(f_fixed, path)?;
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
    if let Some(blank_after_allowed_non_copyright_first_line) =
        blank_after_allowed_non_copyright_first_line
    {
        if !blank_after_allowed_non_copyright_first_line {
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
