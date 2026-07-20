// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Declarative parsing of the comma-separated `key=value` option strings used
//! throughout the OpenVMM CLI (e.g. `--nvme id=nvme0,pcie_port=p0`).
//!
//! This crate is OpenVMM-specific by design, not a general-purpose argument
//! parser: it is expected to accrete VMM-oriented value types and conventions
//! (such as [`MemorySize`]) as more of the CLI moves onto it.
//!
//! Instead of hand-writing a `FromStr` impl that tokenizes the string, matches
//! keys, and produces bespoke error messages, annotate a struct with
//! [`macro@KeyValueArgs`] and let the derive generate a consistent parser:
//!
//! ```
//! use vmm_cli::KeyValueArgs;
//!
//! #[derive(KeyValueArgs)]
//! struct Example {
//!     // required scalar; parsed via the field type's `FromStr`.
//!     name: String,
//!     // boolean flag: `fast`, `fast=on`, and `fast=off` are all accepted.
//!     #[kv(flag)]
//!     fast: bool,
//!     // tri-state boolean: absent => None, `turbo`/`turbo=on` => Some(true),
//!     // `turbo=off` => Some(false).
//!     #[kv(flag)]
//!     turbo: Option<bool>,
//!     // optional scalar; absent => `None`.
//!     count: Option<u32>,
//!     // absent => `Default::default()`.
//!     #[kv(default)]
//!     level: u8,
//! }
//!
//! let e: Example = "name=foo,fast,turbo=off,count=3".parse().unwrap();
//! assert_eq!(e.name, "foo");
//! assert!(e.fast);
//! assert_eq!(e.turbo, Some(false));
//! assert_eq!(e.count, Some(3));
//! assert_eq!(e.level, 0);
//! ```
//!
//! Booleans are spelled uniformly: any `#[kv(flag)]` field accepts bare
//! presence, `=on`, or `=off`, so users never have to remember which options
//! take a value.
//!
//! Mutually-exclusive keys that resolve to one enum variant ("one of"
//! semantics) are expressed with [`macro@KeyValueGroup`] on an enum and a
//! `#[kv(flatten)]` field in the parent struct.
//!
//! `#[kv(flatten)]` also merges a shared `#[derive(KeyValueArgs)]` sub-struct's
//! keys into the parent, so common option sets can be defined once and reused
//! across several parsers.
//!
//! A single leading positional token (the first comma-separated part, taken
//! verbatim) can be captured with `#[kv(positional)]` on one field.

#![forbid(unsafe_code)]

use anyhow::Context;
use std::fmt::Display;
use std::str::FromStr;

#[doc(hidden)]
pub use anyhow;
pub use vmm_cli_derive::KeyValueArgs;
pub use vmm_cli_derive::KeyValueGroup;

/// The error type produced by generated parsers (an [`anyhow::Error`]).
pub type Error = anyhow::Error;
/// Result alias used by generated parsers.
pub type Result<T> = anyhow::Result<T>;

/// A composable set of `key=value` fields that can be accumulated one token at
/// a time and finished into a value.
///
/// Implemented by both `#[derive(KeyValueArgs)]` (a struct of fields) and
/// `#[derive(KeyValueGroup)]` (an enum of mutually-exclusive keys). A parent
/// struct field tagged `#[kv(flatten)]` merges the flattened type's keys into
/// the parent's own.
pub trait KeyValueFields: Sized {
    /// Accumulator state threaded through parsing.
    type Accum: Default;

    /// Try to consume `key`; returns `Ok(true)` if it belongs to this type,
    /// `Ok(false)` if the key is unknown to it.
    fn accept(accum: &mut Self::Accum, key: &str, value: Option<&str>) -> Result<bool>;

    /// Finish parsing, enforcing required/exclusivity constraints and applying
    /// defaults.
    fn finish(accum: Self::Accum) -> Result<Self>;
}

/// Construct an [`Error`] with the given message.
///
/// Used by generated code so it never has to reference `anyhow` macros
/// directly.
#[doc(hidden)]
pub fn error(msg: impl Display) -> Error {
    anyhow::Error::msg(msg.to_string())
}

/// Split a `key=value[,key=value...]` option string on top-level commas,
/// honoring `[...]` nesting so that a bracketed list value containing commas
/// (e.g. `vps=[0,1,4-5]`) is not split apart.
pub fn split_options(s: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                anyhow::ensure!(depth > 0, "unmatched ']' in '{s}'");
                depth -= 1;
            }
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    anyhow::ensure!(depth == 0, "unmatched '[' in '{s}'");
    parts.push(&s[start..]);
    Ok(parts)
}

/// Split one option part into its key and optional value: `a=b` => `("a",
/// Some("b"))`, `a` => `("a", None)`.
pub fn split_kv(part: &str) -> (&str, Option<&str>) {
    match part.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (part, None),
    }
}

/// Parse a required, non-empty value for `key` via `T`'s [`FromStr`].
///
/// An absent or empty value is an error (empty is treated as "no value").
pub fn parse_value<T>(key: &str, value: Option<&str>) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    let value = match value {
        Some(v) if !v.is_empty() => v,
        _ => anyhow::bail!("option '{key}' requires a value"),
    };
    value
        .parse::<T>()
        .map_err(|e| error(format!("invalid value for option '{key}': {e}")))
}

/// Parse an optional value: a bare key or empty value yields `None`, otherwise
/// the value is parsed via `T`'s [`FromStr`].
pub fn parse_opt_value<T>(value: Option<&str>) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: Display,
{
    match value {
        Some(v) if !v.is_empty() => Ok(Some(
            v.parse::<T>()
                .map_err(|e| error(format!("invalid value: {e}")))?,
        )),
        _ => Ok(None),
    }
}

/// Parse the leading positional token (the first comma-separated part, taken
/// verbatim without splitting on `=`) via `T`'s [`FromStr`].
///
/// An absent or empty token is an error.
pub fn parse_positional<T>(name: &str, value: Option<&str>) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    let value = match value {
        Some(v) if !v.is_empty() => v,
        _ => anyhow::bail!("missing required '{name}'"),
    };
    value
        .parse::<T>()
        .map_err(|e| error(format!("invalid '{name}': {e}")))
}

/// Store a parsed value into `slot`, erroring if `key` was already seen.
pub fn set_once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<()> {
    if slot.is_some() {
        anyhow::bail!("duplicate option '{key}'");
    }
    *slot = Some(value);
    Ok(())
}

/// Parse a boolean flag/toggle into `slot`, erroring on duplicates.
///
/// Accepts three uniform spellings so callers never have to remember which
/// booleans are bare and which take a value: bare presence (`foo`) and `foo=on`
/// both mean `true`, and `foo=off` means `false`.
pub fn set_toggle(slot: &mut Option<bool>, key: &str, value: Option<&str>) -> Result<()> {
    let v = match value {
        None | Some("on") => true,
        Some("off") => false,
        Some(other) => {
            anyhow::bail!("flag '{key}' expects no value, 'on', or 'off', got '{other}'")
        }
    };
    if slot.is_some() {
        anyhow::bail!("duplicate flag '{key}'");
    }
    *slot = Some(v);
    Ok(())
}

fn strip_brackets(s: &str) -> Result<&str> {
    s.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .with_context(|| format!("expected bracket syntax like [a,b,c], got '{s}'"))
}

/// A bracket-delimited list value, e.g. `[a,b,c]` or the empty list `[]`.
///
/// Implements [`FromStr`], so it composes directly as a `#[derive(KeyValueArgs)]`
/// field type; because [`split_options`] is bracket-aware, a `BracketList`
/// value may contain commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BracketList<T>(pub Vec<T>);

impl<T> FromStr for BracketList<T>
where
    T: FromStr,
    T::Err: Display,
{
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let inner = strip_brackets(s)?;
        if inner.is_empty() {
            return Ok(Self(Vec::new()));
        }
        let mut items = Vec::new();
        for item in inner.split(',') {
            let item = item.trim();
            items.push(
                item.parse::<T>()
                    .map_err(|e| error(format!("invalid list item '{item}': {e}")))?,
            );
        }
        Ok(Self(items))
    }
}

/// A bracket-delimited list of unsigned integers that also accepts inclusive
/// ranges, e.g. `[0,1,4-5]` expands to `[0, 1, 4, 5]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BracketRangeList(pub Vec<u32>);

impl FromStr for BracketRangeList {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let inner = strip_brackets(s)?;
        if inner.is_empty() {
            return Ok(Self(Vec::new()));
        }
        let mut items = Vec::new();
        for item in inner.split(',') {
            let item = item.trim();
            if let Some((lo, hi)) = item.split_once('-') {
                let lo = lo.trim().parse::<u32>().context("invalid range bound")?;
                let hi = hi.trim().parse::<u32>().context("invalid range bound")?;
                anyhow::ensure!(lo <= hi, "invalid range {lo}-{hi}");
                items.extend(lo..=hi);
            } else {
                items.push(item.parse::<u32>().context("invalid list item")?);
            }
        }
        Ok(Self(items))
    }
}

/// A memory/byte size such as `2G`, `64M`, `512K`, `4T`, or a bare byte count
/// like `1024`. Suffixes `K`/`M`/`G`/`T` are powers of 1024, and an optional
/// trailing `B` is accepted (e.g. `64MB` == `64M`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemorySize(pub u64);

impl FromStr for MemorySize {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut b = s.as_bytes();
        if b.last() == Some(&b'B') {
            b = &b[..b.len() - 1];
        }
        anyhow::ensure!(!b.is_empty(), "invalid memory size '{s}'");
        let multiplier = match b[b.len() - 1] {
            b'T' => Some(1024 * 1024 * 1024 * 1024),
            b'G' => Some(1024 * 1024 * 1024),
            b'M' => Some(1024 * 1024),
            b'K' => Some(1024),
            _ => None,
        };
        if multiplier.is_some() {
            b = &b[..b.len() - 1];
        }
        let n: u64 = std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("invalid memory size '{s}'"))?;
        let bytes = n
            .checked_mul(multiplier.unwrap_or(1))
            .with_context(|| format!("memory size '{s}' overflows"))?;
        Ok(MemorySize(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_options_bracket_aware() {
        assert_eq!(split_options("a=1,b=2").unwrap(), ["a=1", "b=2"]);
        assert_eq!(
            split_options("size=2G,vps=[0,1,4-5]").unwrap(),
            ["size=2G", "vps=[0,1,4-5]"]
        );
        assert!(split_options("a=[1,2").is_err());
        assert!(split_options("a=1]").is_err());
    }

    #[test]
    fn bracket_list() {
        assert_eq!(
            "[a,b,c]".parse::<BracketList<String>>().unwrap().0,
            ["a", "b", "c"]
        );
        assert_eq!("[]".parse::<BracketList<u32>>().unwrap().0, [] as [u32; 0]);
        assert!("a,b".parse::<BracketList<String>>().is_err());
        assert!("[1,x]".parse::<BracketList<u32>>().is_err());
    }

    #[test]
    fn bracket_range_list() {
        assert_eq!(
            "[0,1,4-5]".parse::<BracketRangeList>().unwrap().0,
            [0, 1, 4, 5]
        );
        assert_eq!("[]".parse::<BracketRangeList>().unwrap().0, [] as [u32; 0]);
        assert!("[5-1]".parse::<BracketRangeList>().is_err());
    }

    #[test]
    fn memory_size() {
        assert_eq!("1024".parse::<MemorySize>().unwrap().0, 1024);
        assert_eq!("512K".parse::<MemorySize>().unwrap().0, 512 * 1024);
        assert_eq!("64M".parse::<MemorySize>().unwrap().0, 64 * 1024 * 1024);
        assert_eq!(
            "2G".parse::<MemorySize>().unwrap().0,
            2 * 1024 * 1024 * 1024
        );
        assert_eq!("64MB".parse::<MemorySize>().unwrap().0, 64 * 1024 * 1024);
        assert!("".parse::<MemorySize>().is_err());
        assert!("aG".parse::<MemorySize>().is_err());
        assert!("bad".parse::<MemorySize>().is_err());
    }
}
