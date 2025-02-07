// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines a fixed-size string format that's used by SCSI and NVMe
//! specifications.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use inspect::Inspect;
use mesh_protobuf::Protobuf;
use std::str::FromStr;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A fixed-size string that is padded out with ASCII spaces.
#[derive(Copy, Clone, Protobuf, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(transparent)]
#[mesh(transparent)]
pub struct AsciiString<const N: usize>([u8; N]);

impl<const N: usize> std::fmt::Debug for AsciiString<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(s) = self.as_str() {
            s.fmt(f)
        } else {
            self.as_bytes().fmt(f)
        }
    }
}

impl<const N: usize> Inspect for AsciiString<N> {
    fn inspect(&self, req: inspect::Request<'_>) {
        if let Some(s) = self.as_str() {
            req.value(s.into())
        } else {
            req.value(self.as_bytes().to_vec().into())
        }
    }
}

/// An error returned by [`AsciiString::new`].
#[derive(Debug, Error)]
pub enum InvalidAsciiString {
    /// The string exceeds the length of the buffer.
    #[error("string is too long")]
    TooLong,
    /// The string contains a character outside of `0x20..=0x7f`.
    #[error("string contains non-ascii character")]
    NonAscii,
}

impl<const N: usize> AsciiString<N> {
    /// Returns a new string by padding `name` with spaces.
    ///
    /// Returns `None` if `name` is longer than `N`.
    pub fn new(s: &str) -> Result<Self, InvalidAsciiString> {
        // Pad with spaces.
        let mut b = [b' '; N];
        if !s.bytes().all(|c| matches!(c, 0x20..=0x7f)) {
            return Err(InvalidAsciiString::NonAscii);
        }
        b.get_mut(..s.len())
            .ok_or(InvalidAsciiString::TooLong)?
            .copy_from_slice(s.as_bytes());

        Ok(Self(b))
    }

    /// Gets the string, trimming trailing ASCII spaces.
    ///
    /// Returns `None` if the string is not valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        Some(std::str::from_utf8(&self.0).ok()?.trim_end_matches(' '))
    }

    /// Gets the string as bytes, including the trailing spaces.
    pub fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }
}

impl<const N: usize> From<[u8; N]> for AsciiString<N> {
    fn from(value: [u8; N]) -> Self {
        Self(value)
    }
}

impl<const N: usize> From<AsciiString<N>> for [u8; N] {
    fn from(value: AsciiString<N>) -> Self {
        value.0
    }
}

impl<const N: usize> FromStr for AsciiString<N> {
    type Err = InvalidAsciiString;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}
