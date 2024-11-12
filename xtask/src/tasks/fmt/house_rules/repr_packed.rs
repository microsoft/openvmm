// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;

pub fn check_repr_packed(path: &Path, fix: bool) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(ext, "rs") {
        return Ok(());
    }

    let mut needs_fixing = false;
    let f = BufReader::new(File::open(path)?);
    for (i, line) in f.lines().enumerate() {
        let line = line?;
        if line.trim() == "#[repr(packed)]" {
            needs_fixing = true;

            let msg = format!("#[repr(packed)]: {}:{}", path.display(), i + 1);
            if fix {
                log::info!("fixing {}", msg);
            } else {
                log::error!("found {}", msg);
            }
        }
    }

    if fix && needs_fixing {
        let path_fix = &{
            let mut p = path.to_path_buf();
            let ok = p.set_extension(format!("{}.fix", ext));
            assert!(ok);
            p
        };

        let f = BufReader::new(File::open(path)?);
        let mut f_fixed = File::create(path_fix)?;

        for line in f.lines() {
            let line = line?;
            if line.trim() == "#[repr(packed)]" {
                let whitespace = line.split('#').next().unwrap();
                writeln!(f_fixed, "{whitespace}#[repr(C, packed)]")?;
            } else {
                writeln!(f_fixed, "{}", line)?;
            }
        }

        // swap the file with the newly fixed file
        fs_err::rename(path_fix, path)?;
    }

    if needs_fixing && !fix {
        Err(anyhow!(
            "found uses of #[repr(packed)] in {}",
            path.display()
        ))
    } else {
        Ok(())
    }
}
