// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to manage a filter on addresses.
//!
//! This is used to track the conditions under which a given PIO/MMIO access
//! or interrupt vector should be traced.
//!
//! See [`AddressFilter`] for more information, including the syntax used to
//! define the filter.

use inspect::InspectMut;
use std::fmt::Display;
use std::fmt::LowerHex;
use std::num::ParseIntError;
use std::str::FromStr;
use thiserror::Error;

/// A trait for types that can be used as a range value in an [`AddressFilter`].
pub trait RangeKey: PartialOrd + Ord + PartialEq + Copy + LowerHex {
    /// The zero value for this type.
    const ZERO: Self;
    /// The maximum value for this type.
    const MAX: Self;
    /// Parse a hex string into this type.
    fn parse_hex(s: &str) -> Result<Self, ParseIntError>;
}

impl RangeKey for u8 {
    const ZERO: Self = 0;
    const MAX: Self = Self::MAX;
    fn parse_hex(s: &str) -> Result<Self, ParseIntError> {
        Self::from_str_radix(s, 16)
    }
}

impl RangeKey for u16 {
    const ZERO: Self = 0;
    const MAX: Self = Self::MAX;
    fn parse_hex(s: &str) -> Result<Self, ParseIntError> {
        Self::from_str_radix(s, 16)
    }
}

impl RangeKey for u32 {
    const ZERO: Self = 0;
    const MAX: Self = Self::MAX;
    fn parse_hex(s: &str) -> Result<Self, ParseIntError> {
        Self::from_str_radix(s, 16)
    }
}

impl RangeKey for u64 {
    const ZERO: Self = 0;
    const MAX: Self = Self::MAX;
    fn parse_hex(s: &str) -> Result<Self, ParseIntError> {
        Self::from_str_radix(s, 16)
    }
}

/// A filter over addresses.
///
/// These are primary constructed by parsing a filter string. The string is a
/// comma-delimited list of addresses (0x-prefixed hex values), inclusive ranges
/// (address1-address2), or the `?` character. `?` indicates that addresses that
/// are not recognized as part of the chipset should be filtered.
///
/// For example: `0x40-0x4f,0x63,?` would filter addresses 0x40 through 0x4f, address
/// 0x63, and any unknown addresses.
#[derive(Debug)]
pub struct AddressFilter<T> {
    unknown: bool,
    ranges: Vec<(T, T)>,
}

impl<T> AddressFilter<T>
where
    T: PartialOrd + Ord,
{
    /// Creates an empty filter.
    ///
    /// If `unknown` is true, then unknown addresses should be filtered.
    pub fn new(unknown: bool) -> Self {
        Self {
            ranges: Vec::new(),
            unknown,
        }
    }

    /// Returns whether `address` is in the filter.
    ///
    /// This is true when the address is in one of the ranges, or if `is_known`
    /// and the filter is configured for unknown addresses,
    pub fn filtered(&self, address: &T, is_known: bool) -> bool {
        if !is_known && self.unknown {
            return true;
        }
        // Fast path.
        if self.ranges.is_empty() {
            return false;
        }
        let i = self
            .ranges
            .binary_search_by(|(_start, end)| end.cmp(address))
            .unwrap_or_else(|x| x);
        self.ranges
            .get(i)
            .is_some_and(|(start, end)| address >= start && address <= end)
    }
}

impl<T: RangeKey> Display for AddressFilter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ranges == [(T::ZERO, T::MAX)] {
            return f.write_str("all");
        }
        let mut comma = "";
        for (start, end) in &self.ranges {
            if start == end {
                write!(f, "{comma}{start:#x}")?;
            } else {
                write!(f, "{comma}{start:#x}-{end:#x}")?;
            }
            comma = ",";
        }
        if self.unknown {
            write!(f, "{comma}?")?;
        }
        Ok(())
    }
}

/// Errors returned when trying to parse a full filter string
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum InvalidRangeSet {
    #[error("invalid range value")]
    InvalidValue(#[from] InvalidRangeValue),
    #[error("end before start")]
    EndBeforeStart,
    #[error("overlapping ranges")]
    Overlapping,
}

/// Errors returned when trying to parse a single range value
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum InvalidRangeValue {
    #[error(transparent)]
    ParseInt(#[from] ParseIntError),
    #[error("value must begin with 0x")]
    NotHex,
}

fn parse_value<T: RangeKey>(s: &str) -> Result<T, InvalidRangeValue> {
    Ok(T::parse_hex(
        s.strip_prefix("0x").ok_or(InvalidRangeValue::NotHex)?,
    )?)
}

impl<T: RangeKey> FromStr for AddressFilter<T> {
    type Err = InvalidRangeSet;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut v = Vec::new();
        let mut unknown = false;
        for range in s.split(',') {
            if range == "?" {
                unknown = true;
            } else if s == "all" {
                unknown = true;
                v.push((T::ZERO, T::MAX));
            } else {
                let (start, end) = if let Some((start, end)) = range.split_once('-') {
                    let start = parse_value(start)?;
                    let end = parse_value(end)?;
                    if end < start {
                        return Err(InvalidRangeSet::EndBeforeStart);
                    }
                    (start, end)
                } else {
                    let start = parse_value(range)?;
                    (start, start)
                };
                v.push((start, end));
            }
        }
        v.sort();
        for ((_, end1), (start2, _)) in v.iter().zip(v.iter().skip(1)) {
            if end1 >= start2 {
                return Err(InvalidRangeSet::Overlapping);
            }
        }
        Ok(Self { ranges: v, unknown })
    }
}

impl<T: RangeKey> InspectMut for AddressFilter<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        match req.update() {
            Ok(req) => match req.new_value().parse() {
                Ok(v) => {
                    *self = v;
                    req.succeed(self.to_string());
                }
                Err(err) => {
                    req.fail(err);
                }
            },
            Err(req) => req.value(self.to_string()),
        }
    }
}
