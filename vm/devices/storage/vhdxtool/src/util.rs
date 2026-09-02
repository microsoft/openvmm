// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Size formatting, parsing, and parent-path helpers used by the CLI.

use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn parse_size(value: &str) -> anyhow::Result<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split);
    let number: u64 = number.parse().context("size must start with an integer")?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => anyhow::bail!("unsupported size suffix '{suffix}'"),
    };
    number.checked_mul(multiplier).context("size is too large")
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("TiB", 1024_u64.pow(4)),
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024),
    ];
    for (unit, multiplier) in UNITS {
        if bytes >= *multiplier && bytes.is_multiple_of(*multiplier) {
            return format!("{} {unit}", bytes / multiplier);
        }
    }
    format!("{bytes} bytes")
}

pub(crate) fn relative_path(from_directory: &Path, to: &Path) -> anyhow::Result<PathBuf> {
    let from_directory = fs_err::canonicalize(from_directory)
        .with_context(|| format!("failed to resolve {}", from_directory.display()))?;
    let to =
        fs_err::canonicalize(to).with_context(|| format!("failed to resolve {}", to.display()))?;
    let from_components: Vec<_> = from_directory.components().collect();
    let to_components: Vec<_> = to.components().collect();
    let shared = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    anyhow::ensure!(shared > 0, "child and parent paths have no common root");

    let mut result = PathBuf::new();
    for _ in shared..from_components.len() {
        result.push("..");
    }
    for component in &to_components[shared..] {
        result.push(component.as_os_str());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("4GiB").unwrap(), 4 * 1024 * 1024 * 1024);
    }
}
