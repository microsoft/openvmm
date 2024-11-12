// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use fs_err::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;

pub fn check_trailing_newline(path: &Path, fix: bool) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(
        ext,
        "c" | "md" | "proto" | "py" | "rs" | "sh" | "toml" | "txt" | "yml" | "js" | "ts"
    ) {
        return Ok(());
    }

    // workaround for `mdbook-docfx` emitting yaml with no trailing newline
    if path.file_name().unwrap() == "toc.yml" {
        return Ok(());
    }

    let mut f = OpenOptions::new().read(true).write(fix).open(path)?;
    f.seek(SeekFrom::End(-2))?;
    let mut b = [0; 2];
    f.read_exact(&mut b)?;

    let missing_single_trailing_newline = !(b[0] != b'\n' && b[1] == b'\n');

    if missing_single_trailing_newline {
        if fix {
            let truncate_to = find_first_trailing_nl(&mut f)?;
            f.set_len(truncate_to)?;
            f.seek(SeekFrom::End(0))?;
            writeln!(f)?;
        } else {
            // just report the error
            return Err(anyhow!(
                "missing single trailing newline in {}",
                path.display()
            ));
        }
    }

    Ok(())
}

// implementing this function efficiently requires reading the file backwards,
// which is kinda annoying...
fn find_first_trailing_nl(f: &mut File) -> std::io::Result<u64> {
    const BLOCK_SIZE: u64 = 512;

    let mut pos = f.seek(SeekFrom::End(0))?;
    let mut file_block = [0; BLOCK_SIZE as usize];
    while pos != 0 {
        let new_pos = pos.saturating_sub(BLOCK_SIZE);
        let delta = pos - new_pos;
        pos = new_pos;

        let file_block = &mut file_block[..delta as usize];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(file_block)?;

        let num_trailing_newlines =
            file_block.iter().rev().take_while(|x| **x == b'\n').count() as u64;

        match num_trailing_newlines {
            0 => {
                // no trailing newlines in this block at all
                pos += delta;
                break;
            }
            n if n == delta => {
                // it's all newlines, so we keep on going
            }
            n => {
                // nice, we found the start of the newlines
                pos += delta - n;
                break;
            }
        }
    }

    Ok(pos)
}
