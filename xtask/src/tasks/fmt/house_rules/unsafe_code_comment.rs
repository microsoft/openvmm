// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

pub fn check_unsafe_code_comment(path: &Path, _fix: bool) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(ext, "rs") {
        return Ok(());
    }

    // need to exclude self (and house_rules.rs, which includes help-text) from the lint
    if path == Path::new(file!()) || path == Path::new(super::PATH_TO_HOUSE_RULES_RS) {
        return Ok(());
    }

    let mut error = false;

    // TODO: this lint really ought to be a dynlint / clippy lint
    let f = BufReader::new(File::open(path)?);
    let mut in_comment = false;

    for (i, line) in f.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.starts_with("// UNSAFETY: ") {
            in_comment = true;
            continue;
        }

        if line.contains("allow(unsafe_code)") && !in_comment {
            error = true;
            log::error!(
                "unjustified `allow(unsafe_code)`: {}:{}",
                path.display(),
                i + 1
            );
        }

        if !line.starts_with("//") || (line.len() > 2 && line.as_bytes()[2] != b' ') {
            in_comment = false;
        }
    }

    if error {
        Err(anyhow!(
            "found unjustified uses of `allow(unsafe_code)` in {}",
            path.display()
        ))
    } else {
        Ok(())
    }
}
