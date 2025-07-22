// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MAC address type for sending across mesh channels and displaying with
//! `inspect`.

use inspect::Inspect;
use mesh::payload::Protobuf;
use std::fmt::Display;
use std::str::FromStr;
use thiserror::Error;

/// A 48-bit Ethernet MAC address.
#[derive(Debug, Protobuf, Inspect, Clone, Copy, PartialEq, Eq, Hash)]
#[mesh(transparent)]
#[inspect(display)]
#[repr(transparent)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// Returns a new MAC address from the given bytes.
    pub const fn new(value: [u8; 6]) -> Self {
        Self(value)
    }

    /// Returns the bytes of the MAC address.
    pub const fn to_bytes(self) -> [u8; 6] {
        self.0
    }
}

impl From<[u8; 6]> for MacAddress {
    fn from(value: [u8; 6]) -> Self {
        Self::new(value)
    }
}

impl From<MacAddress> for [u8; 6] {
    fn from(value: MacAddress) -> Self {
        value.0
    }
}

impl Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Error returned when parsing a [`MacAddress`] fails.
#[derive(Debug, Error)]
#[error("invalid mac address")]
pub struct InvalidMacAddress;

impl FromStr for MacAddress {
    type Err = InvalidMacAddress;

    fn from_str(val: &str) -> Result<Self, InvalidMacAddress> {
        if val.len() != 17 {
            return Err(InvalidMacAddress);
        }
        let sep = val.as_bytes()[2];
        if sep != b'-' && sep != b':' {
            return Err(InvalidMacAddress);
        }
        let mut mac_address = [0u8; 6];
        for (src, dst) in val.split(sep as char).zip(&mut mac_address) {
            if src.len() != 2 {
                return Err(InvalidMacAddress);
            }
            *dst = u8::from_str_radix(src, 16).map_err(|_| InvalidMacAddress)?;
        }
        Ok(MacAddress(mac_address))
    }
}

#[cfg(test)]
mod tests {
    use crate::mac_address::InvalidMacAddress;
    use crate::mac_address::MacAddress;
    use std::str::FromStr;

    #[test]
    fn test_parse_mac_address() {
        let bad_macs = &[
            "",
            "00:00:00-00-00-00",
            "00:00:00:00:00",
            "00:00:00:00:00:00:",
            "00:00:00:00:00:00:00",
            "00:00:00:00:00::0",
            "00:00:00:00:00:0g",
        ];
        for mac in bad_macs {
            assert!(matches!(MacAddress::from_str(mac), Err(InvalidMacAddress)));
        }

        let good_macs = &[
            ("00:00:00:00:00:00", [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            ("00-00-00-00-00-00", [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            ("ff:ff:ff:ff:ff:ff", [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            ("FF:FF:FF:FF:FF:FF", [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            ("01:23:45:67:89:ab", [0x01, 0x23, 0x45, 0x67, 0x89, 0xab]),
            ("01-23-45-67-89-ab", [0x01, 0x23, 0x45, 0x67, 0x89, 0xab]),
        ];
        for &(mac, parsed) in good_macs {
            assert_eq!(MacAddress::from_str(mac).unwrap().to_bytes(), parsed);
        }
    }
}
