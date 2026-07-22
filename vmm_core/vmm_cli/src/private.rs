// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Error;
use crate::KeyValueFields;
use crate::Result;
use std::fmt::Display;
use std::str::FromStr;

/// Construct an [`Error`] with the given message.
pub fn error(msg: impl Display) -> Error {
    anyhow::Error::msg(msg.to_string())
}

/// Construct an unknown-option error listing all keys accepted by `T`.
pub fn unknown_option<T: KeyValueFields>(key: &str) -> Error {
    let mut keys = Vec::new();
    T::append_keys(&mut keys);
    error(format!(
        "unknown option '{key}'; expected one of: {}",
        keys.iter()
            .map(|key| format!("'{key}'"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Split an option string on top-level commas, honoring `[...]` nesting.
pub fn split_options(s: &str) -> Result<Vec<&str>> {
    if s.is_empty() {
        return Ok(Vec::new());
    }

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
                let part = &s[start..i];
                anyhow::ensure!(!part.is_empty(), "empty option in '{s}'");
                parts.push(part);
                start = i + 1;
            }
            _ => {}
        }
    }
    anyhow::ensure!(depth == 0, "unmatched '[' in '{s}'");
    let part = &s[start..];
    anyhow::ensure!(!part.is_empty(), "empty option in '{s}'");
    parts.push(part);
    Ok(parts)
}

/// Split one option into its non-empty key and optional value.
pub fn split_kv(part: &str) -> Result<(&str, Option<&str>)> {
    let (key, value) = match part.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (part, None),
    };
    anyhow::ensure!(!key.is_empty(), "option key cannot be empty");
    Ok((key, value))
}

/// Parse a required, non-empty value for `key` via `T`'s [`FromStr`].
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

/// Parse an optional value for `key`, returning `None` when `=` is omitted.
pub fn parse_opt_value<T>(key: &str, value: Option<&str>) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: Display,
{
    match value {
        Some("") => anyhow::bail!("option '{key}' requires a value after '='"),
        Some(value) => {
            Ok(Some(value.parse::<T>().map_err(|e| {
                error(format!("invalid value for option '{key}': {e}"))
            })?))
        }
        None => Ok(None),
    }
}

/// Parse the leading positional token via `T`'s [`FromStr`].
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
