// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-width hexadecimal JSON encodings for register values.
//!
//! Register values and bit masks are encoded as `0x`-prefixed strings with a
//! fixed number of digits. Unlike JSON numbers, these round-trip 64-bit
//! values through tools that use double-precision numbers, and they keep
//! fingerprints readable and line-diffable.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use std::fmt;
use thiserror::Error;

/// A malformed hexadecimal register value.
#[derive(Debug, Error)]
#[error("invalid hexadecimal value {value:?}: expected 0x followed by up to {digits} hex digits")]
pub struct ParseHexError {
    value: String,
    digits: usize,
}

fn parse_hex(value: &str, digits: usize) -> Result<u64, ParseHexError> {
    let error = || ParseHexError {
        value: value.to_owned(),
        digits,
    };
    let hex = value.strip_prefix("0x").ok_or_else(error)?;
    if hex.is_empty() || hex.len() > digits || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error());
    }
    u64::from_str_radix(hex, 16).map_err(|_| error())
}

/// A 32-bit register value, encoded as `0x` and eight hex digits.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hex32(pub u32);

impl Hex32 {
    /// Parses `0x` followed by one to eight hex digits.
    pub fn parse(value: &str) -> Result<Self, ParseHexError> {
        // At most eight digits were accepted, so the value fits.
        Ok(Self(parse_hex(value, 8)? as u32))
    }
}

impl fmt::Display for Hex32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#010x}", self.0)
    }
}

impl From<u32> for Hex32 {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<Hex32> for u32 {
    fn from(value: Hex32) -> Self {
        value.0
    }
}

impl Serialize for Hex32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(format_hex(&mut [0; 10], self.0.into()))
    }
}

impl<'de> Deserialize<'de> for Hex32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            deserializer.deserialize_str(HexVisitor { digits: 8 })? as u32,
        ))
    }
}

/// Formats `value` as `0x` and `buffer.len() - 2` hex digits in `buffer`,
/// without the formatting machinery, as profiles hold hundreds of values.
fn format_hex(buffer: &mut [u8], value: u64) -> &str {
    let digits = buffer.len() - 2;
    buffer[..2].copy_from_slice(b"0x");
    for (i, digit) in buffer[2..].iter_mut().enumerate() {
        *digit = b"0123456789abcdef"[((value >> (4 * (digits - 1 - i))) & 0xf) as usize];
    }
    std::str::from_utf8(buffer).expect("hex digits are ASCII")
}

/// Parses a hex string without allocating, as profiles hold hundreds of them.
struct HexVisitor {
    digits: usize,
}

impl serde::de::Visitor<'_> for HexVisitor {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x followed by up to {} hex digits", self.digits)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<u64, E> {
        parse_hex(value, self.digits).map_err(E::custom)
    }
}

/// A 64-bit register value, encoded as `0x` and sixteen hex digits.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hex64(pub u64);

impl Hex64 {
    /// Parses `0x` followed by one to sixteen hex digits.
    pub fn parse(value: &str) -> Result<Self, ParseHexError> {
        Ok(Self(parse_hex(value, 16)?))
    }
}

impl fmt::Display for Hex64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl From<u64> for Hex64 {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hex64> for u64 {
    fn from(value: Hex64) -> Self {
        value.0
    }
}

impl Serialize for Hex64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(format_hex(&mut [0; 18], self.0))
    }
}

impl<'de> Deserialize<'de> for Hex64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            deserializer.deserialize_str(HexVisitor { digits: 16 })?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::Hex32;
    use super::Hex64;
    use test_with_tracing::test;

    #[test]
    fn formats_fixed_width() {
        assert_eq!(Hex32(0x1f).to_string(), "0x0000001f");
        assert_eq!(Hex32(u32::MAX).to_string(), "0xffffffff");
        assert_eq!(Hex64(0x10a).to_string(), "0x000000000000010a");
        assert_eq!(Hex64(u64::MAX).to_string(), "0xffffffffffffffff");
        // Serialization formats the same digits as Display.
        for value in [0, 0x1f, 0x8000_0000, 0x0123_4567, u32::MAX] {
            assert_eq!(
                serde_json::to_string(&Hex32(value)).unwrap(),
                format!("\"{}\"", Hex32(value))
            );
        }
        for value in [0, 0x10a, 0x0123_4567_89ab_cdef, u64::MAX] {
            assert_eq!(
                serde_json::to_string(&Hex64(value)).unwrap(),
                format!("\"{}\"", Hex64(value))
            );
        }
    }

    #[test]
    fn round_trips_through_json() {
        for value in [0, 1, 0x8000_0000, u32::MAX] {
            let json = serde_json::to_string(&Hex32(value)).unwrap();
            assert_eq!(serde_json::from_str::<Hex32>(&json).unwrap(), Hex32(value));
        }
        for value in [0, 1 << 53 | 1, u64::MAX] {
            let json = serde_json::to_string(&Hex64(value)).unwrap();
            assert_eq!(serde_json::from_str::<Hex64>(&json).unwrap(), Hex64(value));
        }
    }

    #[test]
    fn rejects_malformed_values() {
        for value in [
            "",
            "0x",
            "1f",
            "0X1f",
            "0x1g",
            "0x123456789",
            "-0x1",
            "0x 1",
        ] {
            assert!(Hex32::parse(value).is_err(), "{value:?}");
        }
        assert!(Hex64::parse("0x00000000000000001").is_err());
        assert!(serde_json::from_str::<Hex32>("31").is_err());
    }
}
