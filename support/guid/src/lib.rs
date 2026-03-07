// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides the [`Guid`] type with the same layout as the Windows type `GUID`.

#![forbid(unsafe_code)]

use std::str::FromStr;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Windows format GUID.
#[repr(C)]
#[derive(
    Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, IntoBytes, FromBytes, Immutable, KnownLayout,
)]
#[cfg_attr(
    feature = "mesh",
    derive(mesh_protobuf::Protobuf),
    mesh(package = "msguid")
)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(display))]
#[expect(missing_docs)]
pub struct Guid {
    #[cfg_attr(feature = "mesh", mesh(1))]
    pub data1: u32,
    #[cfg_attr(feature = "mesh", mesh(2))]
    pub data2: u16,
    #[cfg_attr(feature = "mesh", mesh(3))]
    pub data3: u16,
    #[cfg_attr(feature = "mesh", mesh(4))]
    pub data4: [u8; 8],
}

// Default + FromBytes: null-guid is a reasonable return default
impl Default for Guid {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

// These two macros are used to work around the fact that ? can't be used in const fn.
macro_rules! option_helper {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => return None,
        }
    };
}

macro_rules! result_helper {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => return Err(ParseError::Digit),
        }
    };
}

/// Creates a new GUID from a string, parsing the GUID at compile time.
/// "{00000000-0000-0000-0000-000000000000}" and
/// "00000000-0000-0000-0000-000000000000".
#[macro_export]
macro_rules! guid {
    ($x:expr $(,)?) => {
        const { $crate::Guid::from_str_private($x) }
    };
}

impl Guid {
    /// Return a new randomly-generated Version 4 UUID
    pub fn new_random() -> Self {
        let mut guid = Guid::default();
        getrandom::fill(guid.as_mut_bytes()).expect("rng failure");

        guid.data3 = guid.data3 & 0xfff | 0x4000;
        // Variant 1
        guid.data4[0] = guid.data4[0] & 0x3f | 0x80;

        guid
    }

    /// Do not use. Use the [`guid`] macro instead.
    #[doc(hidden)]
    pub const fn from_str_private(value: &str) -> Guid {
        // Unwrap and expect are not supported in const fn.
        match Self::parse(value.as_bytes()) {
            Ok(guid) => guid,
            Err(ParseError::Length) => panic!("Invalid GUID length."),
            Err(ParseError::Format) => panic!("Invalid GUID format."),
            Err(ParseError::Digit) => panic!("Invalid GUID digit."),
        }
    }

    /// Helper used by `from_str_private`, `from_str`, and `TryFrom<&[u8]>`.
    const fn parse(value: &[u8]) -> Result<Self, ParseError> {
        // Slicing is not possible in const fn, so use an index offset.
        let offset = if value.len() == 38 {
            if value[0] != b'{' || value[37] != b'}' {
                return Err(ParseError::Format);
            }

            1
        } else if value.len() == 36 {
            0
        } else {
            return Err(ParseError::Length);
        };

        if value[offset + 8] != b'-'
            || value[offset + 13] != b'-'
            || value[offset + 18] != b'-'
            || value[offset + 23] != b'-'
        {
            return Err(ParseError::Format);
        }

        // No for loops in const fn, so do it one at a time.
        Ok(Guid {
            data1: result_helper!(u32_from_hex(value, offset)),
            data2: result_helper!(u16_from_hex(value, offset + 9)),
            data3: result_helper!(u16_from_hex(value, offset + 14)),
            data4: [
                result_helper!(u8_from_hex(value, offset + 19)),
                result_helper!(u8_from_hex(value, offset + 21)),
                result_helper!(u8_from_hex(value, offset + 24)),
                result_helper!(u8_from_hex(value, offset + 26)),
                result_helper!(u8_from_hex(value, offset + 28)),
                result_helper!(u8_from_hex(value, offset + 30)),
                result_helper!(u8_from_hex(value, offset + 32)),
                result_helper!(u8_from_hex(value, offset + 34)),
            ],
        })
    }

    /// The all-zero GUID.
    pub const ZERO: Self = guid!("00000000-0000-0000-0000-000000000000");

    /// Returns true if this is the all-zero GUID.
    pub fn is_zero(&self) -> bool {
        self == &Self::ZERO
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7],
        )
    }
}

impl std::fmt::LowerHex for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::UpperHex for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7],
        )
    }
}

impl std::fmt::Debug for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// An error parsing a GUID.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ParseError {
    #[error("invalid GUID length")]
    Length,
    #[error("invalid GUID format")]
    Format,
    #[error("invalid GUID digit")]
    Digit,
}

const fn char_to_hex(value: u8) -> Option<u8> {
    Some(match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => 10 + value - b'a',
        b'A'..=b'F' => 10 + value - b'A',
        _ => return None,
    })
}

const fn u8_from_hex(input: &[u8], index: usize) -> Option<u8> {
    Some(
        option_helper!(char_to_hex(input[index])) << 4
            | option_helper!(char_to_hex(input[index + 1])),
    )
}

const fn u16_from_hex(input: &[u8], index: usize) -> Option<u16> {
    Some(
        (option_helper!(u8_from_hex(input, index)) as u16) << 8
            | (option_helper!(u8_from_hex(input, index + 2)) as u16),
    )
}

const fn u32_from_hex(input: &[u8], index: usize) -> Option<u32> {
    Some(
        (option_helper!(u16_from_hex(input, index)) as u32) << 16
            | (option_helper!(u16_from_hex(input, index + 4)) as u32),
    )
}

impl FromStr for Guid {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.as_bytes())
    }
}

impl TryFrom<&[u8]> for Guid {
    type Error = ParseError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Guid::parse(value)
    }
}

impl From<Guid> for [u8; 16] {
    fn from(value: Guid) -> Self {
        value.as_bytes().try_into().unwrap()
    }
}

mod windows {
    #![cfg(windows)]
    use super::Guid;

    impl From<Guid> for win_etw_provider::GUID {
        fn from(guid: Guid) -> Self {
            Self {
                data1: guid.data1,
                data2: guid.data2,
                data3: guid.data3,
                data4: guid.data4,
            }
        }
    }

    impl From<windows_sys::core::GUID> for Guid {
        fn from(guid: windows_sys::core::GUID) -> Self {
            Self {
                data1: guid.data1,
                data2: guid.data2,
                data3: guid.data3,
                data4: guid.data4,
            }
        }
    }

    impl From<Guid> for windows_sys::core::GUID {
        fn from(guid: Guid) -> Self {
            Self {
                data1: guid.data1,
                data2: guid.data2,
                data3: guid.data3,
                data4: guid.data4,
            }
        }
    }

    impl From<windows::core::GUID> for Guid {
        fn from(guid: windows::core::GUID) -> Self {
            Self {
                data1: guid.data1,
                data2: guid.data2,
                data3: guid.data3,
                data4: guid.data4,
            }
        }
    }

    impl From<Guid> for windows::core::GUID {
        fn from(guid: Guid) -> Self {
            Self {
                data1: guid.data1,
                data2: guid.data2,
                data3: guid.data3,
                data4: guid.data4,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Guid;

    #[test]
    fn test_display_guid() {
        let guid = Guid {
            data1: 0xcf127acc,
            data2: 0xc960,
            data3: 0x41e4,
            data4: [0x9b, 0x1e, 0x51, 0x3e, 0x8a, 0x89, 0x14, 0x7d],
        };
        assert_eq!(format!("{}", &guid), "cf127acc-c960-41e4-9b1e-513e8a89147d");
    }

    #[test]
    fn test_parse_guid() {
        let guid = Guid {
            data1: 0xcf127acc,
            data2: 0xc960,
            data3: 0x41e4,
            data4: [0x9b, 0x1e, 0x51, 0x3e, 0x8a, 0x89, 0x14, 0x7d],
        };
        assert_eq!(
            guid,
            b"cf127acc-c960-41e4-9b1e-513e8a89147d"[..]
                .try_into()
                .expect("valid GUID")
        );
        assert_eq!(
            guid,
            b"{cf127acc-c960-41e4-9b1e-513e8a89147d}"[..]
                .try_into()
                .expect("valid braced GUID")
        );

        // Test GUID parsing at compile time.
        const TEST_GUID: Guid = guid!("cf127acc-c960-41e4-9b1e-513e8a89147d");
        assert_eq!(guid, TEST_GUID);
        const TEST_BRACED_GUID: Guid = guid!("{cf127acc-c960-41e4-9b1e-513e8a89147d}");
        assert_eq!(guid, TEST_BRACED_GUID);
    }
}
