// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared protocol for the OpenTMK build-time-patchable config region.
//!
//! Guest (`opentmk`) and host tooling (`xtask`, `petri`) share this crate so
//! the embedded [`OpenTmkConfig`] layout and [`TestConfig`] JSON schema stay in sync.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use serde::Deserialize;
use serde::Serialize;

/// Size of the embedded JSON config payload, in bytes.
pub const OPENTMK_CONFIG_JSON_SIZE: usize = 4096;

/// Magic signature marking the start of the embedded [`OpenTmkConfig`] region.
///
/// Host tooling scans for these bytes and patches the region in place. The
/// value is non-zero so the static lands in initialized data, not `.bss`.
pub const OPENTMK_CONFIG_MAGIC: [u8; 16] = *b"OPENTMK_CFG_001\0";

/// Byte offset of `json_len` within the region, relative to the magic.
const OFFSET_JSON_LEN: usize = 16;
/// Byte offset of `json` within the region, relative to the magic.
const OFFSET_JSON: usize = 20;

/// Build-time-patchable config region embedded in the binary.
///
/// The fixed `#[repr(C)]` layout (`magic` | `json_len` | `json`) lets host
/// tooling patch the JSON payload without recompiling.
#[repr(C, align(8))]
pub struct OpenTmkConfig {
    /// Locator signature. See [`OPENTMK_CONFIG_MAGIC`].
    pub magic: [u8; 16],
    /// Number of valid bytes in `json` (little-endian, `<= OPENTMK_CONFIG_JSON_SIZE`).
    pub json_len: u32,
    /// JSON payload (UTF-8). Bytes beyond `json_len` are ignored.
    pub json: [u8; OPENTMK_CONFIG_JSON_SIZE],
}

impl OpenTmkConfig {
    /// An unset config region (valid magic, empty payload) for host patching.
    pub const fn new() -> Self {
        Self {
            magic: OPENTMK_CONFIG_MAGIC,
            json_len: 0,
            json: [0; OPENTMK_CONFIG_JSON_SIZE],
        }
    }

    /// Parses the embedded JSON into a [`TestConfig`], or `None` if unset or
    /// invalid. Never panics.
    pub fn parse(&self) -> Option<TestConfig> {
        let len = self.json_len as usize;
        if len == 0 || len > OPENTMK_CONFIG_JSON_SIZE {
            return None;
        }
        let s = core::str::from_utf8(&self.json[..len]).ok()?;
        serde_json::from_str(s).ok()
    }
}

impl Default for OpenTmkConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Test configuration parsed from the embedded JSON payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TestConfig {
    /// Optional schema version.
    #[serde(default)]
    pub version: u32,
    /// Backend that owns the test, e.g. `"hyperv"`.
    pub backend: String,
    /// Test name within the backend, e.g. `"hv_processor"`.
    pub test: String,
    /// Arbitrary additional parameters (iteration counts, flags, etc.).
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Error returned by [`patch_opentmk_config`].
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// The JSON payload was not valid UTF-8.
    #[error("config JSON is not valid UTF-8")]
    InvalidUtf8,
    /// The JSON payload did not parse as a [`TestConfig`].
    #[error("config JSON did not parse as a TestConfig")]
    InvalidJson,
    /// The JSON payload exceeds [`OPENTMK_CONFIG_JSON_SIZE`].
    #[error("config JSON is {len} bytes, exceeds maximum of {max} bytes")]
    JsonTooLarge {
        /// Length of the supplied JSON.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },
    /// The magic signature was not found in the image.
    #[error("config magic signature not found in image")]
    MagicNotFound,
    /// The magic signature was found more than once, so the target is ambiguous.
    #[error("config magic signature found {0} times; expected exactly one")]
    AmbiguousMagic(usize),
    /// The image is too small to contain the full config region after the magic.
    #[error("image truncated: config region does not fit after magic at offset {0}")]
    Truncated(usize),
}

/// Patches the embedded [`OpenTmkConfig`] region of `image` in place to carry `json`.
///
/// The magic must appear exactly once and `json` must be a valid [`TestConfig`]
/// no larger than [`OPENTMK_CONFIG_JSON_SIZE`]. Works on a raw `.efi` or disk image.
pub fn patch_opentmk_config(image: &mut [u8], json: &[u8]) -> Result<(), PatchError> {
    let s = core::str::from_utf8(json).map_err(|_| PatchError::InvalidUtf8)?;
    serde_json::from_str::<TestConfig>(s).map_err(|_| PatchError::InvalidJson)?;
    if json.len() > OPENTMK_CONFIG_JSON_SIZE {
        return Err(PatchError::JsonTooLarge {
            len: json.len(),
            max: OPENTMK_CONFIG_JSON_SIZE,
        });
    }

    let base = find_magic(image)?;

    let end = base + OFFSET_JSON + OPENTMK_CONFIG_JSON_SIZE;
    if end > image.len() {
        return Err(PatchError::Truncated(base));
    }

    let len = json.len() as u32;
    image[base + OFFSET_JSON_LEN..base + OFFSET_JSON_LEN + 4].copy_from_slice(&len.to_le_bytes());
    let json_start = base + OFFSET_JSON;
    image[json_start..json_start + json.len()].copy_from_slice(json);
    for b in &mut image[json_start + json.len()..json_start + OPENTMK_CONFIG_JSON_SIZE] {
        *b = 0;
    }

    Ok(())
}

/// Finds the single offset of [`OPENTMK_CONFIG_MAGIC`] within `image`.
fn find_magic(image: &[u8]) -> Result<usize, PatchError> {
    let mut base = None;
    let mut count = 0usize;
    if image.len() >= OPENTMK_CONFIG_MAGIC.len() {
        for i in 0..=image.len() - OPENTMK_CONFIG_MAGIC.len() {
            if image[i..i + OPENTMK_CONFIG_MAGIC.len()] == OPENTMK_CONFIG_MAGIC {
                count += 1;
                if base.is_none() {
                    base = Some(i);
                }
            }
        }
    }
    match (base, count) {
        (None, _) => Err(PatchError::MagicNotFound),
        (Some(_), n) if n > 1 => Err(PatchError::AmbiguousMagic(n)),
        (Some(b), _) => Ok(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Builds a minimal image containing exactly one config region.
    fn image_with_region() -> Vec<u8> {
        let mut image = vec![0xAAu8; 64];
        image.extend_from_slice(&OPENTMK_CONFIG_MAGIC);
        image.extend_from_slice(&0u32.to_le_bytes());
        image.extend_from_slice(&[0u8; OPENTMK_CONFIG_JSON_SIZE]);
        image.extend_from_slice(&[0xBBu8; 16]);
        image
    }

    #[test]
    fn patch_round_trips() {
        let mut image = image_with_region();
        let json = br#"{"backend":"hyperv","test":"hv_processor"}"#;
        patch_opentmk_config(&mut image, json).unwrap();

        let base = find_magic(&image).unwrap();
        let json_len = u32::from_le_bytes(
            image[base + OFFSET_JSON_LEN..base + OFFSET_JSON_LEN + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        assert_eq!(json_len, json.len());
        let start = base + OFFSET_JSON;
        let cfg: TestConfig = serde_json::from_slice(&image[start..start + json_len]).unwrap();
        assert_eq!(cfg.backend, "hyperv");
        assert_eq!(cfg.test, "hv_processor");
    }

    #[test]
    fn missing_magic_errors() {
        let mut image = vec![0u8; 128];
        let json = br#"{"backend":"hyperv","test":"t"}"#;
        assert!(matches!(
            patch_opentmk_config(&mut image, json),
            Err(PatchError::MagicNotFound)
        ));
    }

    #[test]
    fn ambiguous_magic_errors() {
        let mut image = image_with_region();
        image.extend_from_slice(&OPENTMK_CONFIG_MAGIC);
        let json = br#"{"backend":"hyperv","test":"t"}"#;
        assert!(matches!(
            patch_opentmk_config(&mut image, json),
            Err(PatchError::AmbiguousMagic(2))
        ));
    }

    #[test]
    fn invalid_json_errors() {
        let mut image = image_with_region();
        assert!(matches!(
            patch_opentmk_config(&mut image, b"not json"),
            Err(PatchError::InvalidJson)
        ));
    }
}
