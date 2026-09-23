// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared protocol for the OpenTMK build-time-patchable config region.
//! Keeps [`OpenTmkConfig`] layout and [`TestConfig`] JSON shared by guest and host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use serde::Deserialize;
use serde::Serialize;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::little_endian::U32;

/// Size of the embedded JSON config payload, in bytes.
pub const OPENTMK_CONFIG_JSON_SIZE: usize = 4096;

/// Magic signature marking the start of the embedded [`OpenTmkConfig`] region.
/// Host patching validates it in [`OPENTMK_CONFIG_SECTION`]; non-zero keeps it out of `.bss`.
pub const OPENTMK_CONFIG_MAGIC: [u8; 16] = *b"OPENTMK_CFG_001\0";

/// Name of the PE section that holds the [`OpenTmkConfig`] region.
/// Must match `#[unsafe(link_section = ".tmkcfg")]` on `OPENTMK_CONFIG`.
pub const OPENTMK_CONFIG_SECTION: &str = ".tmkcfg";

/// Byte offset of `json_len` within the region, relative to the magic.
const OFFSET_JSON_LEN: usize = 16;
/// Byte offset of `json` within the region, relative to the magic.
const OFFSET_JSON: usize = 20;

const _: () = {
    assert!(OFFSET_JSON_LEN == core::mem::offset_of!(OpenTmkConfig, json_len));
    assert!(OFFSET_JSON == core::mem::offset_of!(OpenTmkConfig, json));
};

/// Build-time-patchable config region embedded in the binary.
/// Fixed layout (`magic` | `json_len` | `json`) and alignment let host tooling patch raw bytes.
#[repr(C)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct OpenTmkConfig {
    /// Locator signature. See [`OPENTMK_CONFIG_MAGIC`].
    pub magic: [u8; 16],
    /// Number of valid bytes in `json` (little-endian, `<= OPENTMK_CONFIG_JSON_SIZE`).
    pub json_len: U32,
    /// JSON payload (UTF-8). Bytes beyond `json_len` are ignored.
    pub json: [u8; OPENTMK_CONFIG_JSON_SIZE],
}

impl OpenTmkConfig {
    /// An unset config region (valid magic, empty payload) for host patching.
    pub const fn new() -> Self {
        Self {
            magic: OPENTMK_CONFIG_MAGIC,
            json_len: U32::ZERO,
            json: [0; OPENTMK_CONFIG_JSON_SIZE],
        }
    }

    /// Parses the embedded JSON into a [`TestConfig`], or `None` if unset or
    /// invalid. Never panics.
    pub fn parse(&self) -> Option<TestConfig> {
        if self.magic != OPENTMK_CONFIG_MAGIC {
            return None;
        }
        let len = self.json_len.get() as usize;
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
    /// The image was not a PE, or its config section did not contain the magic.
    #[error("config magic signature not found in image")]
    MagicNotFound,
    /// The config region did not fit within its section after the magic.
    #[error("image truncated: config region does not fit within its section (magic at offset {0})")]
    Truncated(usize),
}

/// Patches the embedded [`OpenTmkConfig`] region of `image` in place to carry `json`.
/// `image` must be raw PE `.efi`; `json` must be valid [`TestConfig`] <= [`OPENTMK_CONFIG_JSON_SIZE`].
pub fn patch_opentmk_config(image: &mut [u8], json: &[u8]) -> Result<(), PatchError> {
    let s = core::str::from_utf8(json).map_err(|_| PatchError::InvalidUtf8)?;
    serde_json::from_str::<TestConfig>(s).map_err(|_| PatchError::InvalidJson)?;
    if json.len() > OPENTMK_CONFIG_JSON_SIZE {
        return Err(PatchError::JsonTooLarge {
            len: json.len(),
            max: OPENTMK_CONFIG_JSON_SIZE,
        });
    }

    // Locate the config region: find the magic anywhere within the `.tmkcfg` PE
    // section, so we don't depend on it being the first thing in the section.
    let (sec_off, sec_len) = config_section_range(image).ok_or(PatchError::MagicNotFound)?;
    let section = &mut image[sec_off..sec_off + sec_len];
    let rel = section
        .windows(OPENTMK_CONFIG_MAGIC.len())
        .position(|w| w == OPENTMK_CONFIG_MAGIC)
        .ok_or(PatchError::MagicNotFound)?;

    // Take a typed view bounded to the section so a match near the section end
    // can't overrun into following sections; a config that doesn't fit within
    // the section is reported as `Truncated`.
    let (region, _rest) = OpenTmkConfig::mut_from_prefix(&mut section[rel..])
        .map_err(|_| PatchError::Truncated(sec_off + rel))?;
    region.json_len = U32::new(json.len() as u32);
    region.json[..json.len()].copy_from_slice(json);
    region.json[json.len()..].fill(0);

    Ok(())
}

/// Reads a little-endian `u16` at `off`, or `None` if out of bounds.
fn read_u16_le(image: &[u8], off: usize) -> Option<u16> {
    let b = image.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Reads a little-endian `u32` at `off`, or `None` if out of bounds.
fn read_u32_le(image: &[u8], off: usize) -> Option<u32> {
    let b = image.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Locates the raw file range of [`OPENTMK_CONFIG_SECTION`] in a PE `.efi`,
/// or `None` if the image is not PE or lacks the section.
fn config_section_range(image: &[u8]) -> Option<(usize, usize)> {
    // DOS header: the PE header offset is a `u32` at 0x3C.
    let pe = read_u32_le(image, 0x3C)? as usize;
    // PE signature.
    if image.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    // The COFF file header follows the 4-byte signature; section headers follow
    // the 20-byte COFF header and the variable-size optional header.
    let coff = pe.checked_add(4)?;
    let num_sections = read_u16_le(image, coff.checked_add(2)?)? as usize;
    let opt_header_size = read_u16_le(image, coff.checked_add(16)?)? as usize;
    let sections = coff.checked_add(20)?.checked_add(opt_header_size)?;

    // PE section names are 8 bytes, null-padded.
    let mut want = [0u8; 8];
    let name = OPENTMK_CONFIG_SECTION.as_bytes();
    want.get_mut(..name.len())?.copy_from_slice(name);

    for i in 0..num_sections {
        // Each section header is 40 bytes: name[8], then `SizeOfRawData` at
        // offset 16 and `PointerToRawData` at offset 20.
        let sh = sections.checked_add(i.checked_mul(40)?)?;
        if image.get(sh..sh.checked_add(8)?)? == want {
            let raw_size = read_u32_le(image, sh.checked_add(16)?)? as usize;
            let raw_ptr = read_u32_le(image, sh.checked_add(20)?)? as usize;
            if raw_ptr.checked_add(raw_size)? > image.len() {
                return None;
            }
            return Some((raw_ptr, raw_size));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Builds a minimal PE image whose `.tmkcfg` section holds one config region,
    /// preceded by `lead_pad` filler bytes within the section.
    fn pe_with_region(lead_pad: usize) -> Vec<u8> {
        // The config region: magic | json_len = 0 | json[JSON_SIZE].
        let mut region = Vec::new();
        region.extend_from_slice(&OPENTMK_CONFIG_MAGIC);
        region.extend_from_slice(&0u32.to_le_bytes());
        region.extend_from_slice(&[0u8; OPENTMK_CONFIG_JSON_SIZE]);

        // Section data = leading filler (not the magic) followed by the region.
        let mut section = vec![0xAAu8; lead_pad];
        section.extend_from_slice(&region);

        let pe = 64usize; // e_lfanew
        let opt_header_size = 0usize; // no optional header, for simplicity
        let coff = pe + 4;
        let sections = coff + 20 + opt_header_size;
        let raw_ptr = sections + 40; // section data follows the one section header

        let mut image = vec![0u8; raw_ptr];
        image[0x3C..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // num_sections
        image[coff + 16..coff + 18].copy_from_slice(&(opt_header_size as u16).to_le_bytes());
        let sh = sections;
        image[sh..sh + 8].copy_from_slice(b".tmkcfg\0");
        image[sh + 16..sh + 20].copy_from_slice(&(section.len() as u32).to_le_bytes());
        image[sh + 20..sh + 24].copy_from_slice(&(raw_ptr as u32).to_le_bytes());
        image.extend_from_slice(&section);
        image
    }

    /// Builds a minimal PE image whose `.tmkcfg` section starts with the config.
    fn image_with_region() -> Vec<u8> {
        pe_with_region(0)
    }

    /// Finds the config offset the way `patch_opentmk_config` does: the magic
    /// within the `.tmkcfg` section.
    fn config_base(image: &[u8]) -> usize {
        let (sec_off, sec_len) = config_section_range(image).unwrap();
        let rel = image[sec_off..sec_off + sec_len]
            .windows(OPENTMK_CONFIG_MAGIC.len())
            .position(|w| w == OPENTMK_CONFIG_MAGIC)
            .unwrap();
        sec_off + rel
    }

    #[test]
    fn patch_round_trips() {
        let mut image = image_with_region();
        let json = br#"{"backend":"hyperv","test":"hv_processor"}"#;
        patch_opentmk_config(&mut image, json).unwrap();

        let base = config_base(&image);
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
    fn config_not_at_section_start() {
        // The config sits 128 bytes into `.tmkcfg`; patching must still find it.
        let mut image = pe_with_region(128);
        let json = br#"{"backend":"hyperv","test":"hv_processor"}"#;
        patch_opentmk_config(&mut image, json).unwrap();

        let base = config_base(&image);
        let start = base + OFFSET_JSON;
        let cfg: TestConfig = serde_json::from_slice(&image[start..start + json.len()]).unwrap();
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
    fn config_overrunning_section_is_truncated() {
        // A `.tmkcfg` section that contains the magic but is too small to hold
        // the full config, followed by ample trailing file bytes. Patching must
        // report `Truncated` rather than overrun into the trailing bytes.
        let pe = 64usize;
        let coff = pe + 4;
        let sections = coff + 20;
        let raw_ptr = sections + 40;
        let sec_len = OPENTMK_CONFIG_MAGIC.len() + 16; // magic + filler, far < region

        let mut image = vec![0u8; raw_ptr];
        image[0x3C..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&0u16.to_le_bytes());
        image[sections..sections + 8].copy_from_slice(b".tmkcfg\0");
        image[sections + 16..sections + 20].copy_from_slice(&(sec_len as u32).to_le_bytes());
        image[sections + 20..sections + 24].copy_from_slice(&(raw_ptr as u32).to_le_bytes());
        image.extend_from_slice(&OPENTMK_CONFIG_MAGIC);
        image.extend_from_slice(&[0u8; 16]);
        // Trailing bytes beyond the section, enough that an unbounded write would
        // have found room for the whole region.
        image.resize(raw_ptr + OPENTMK_CONFIG_JSON_SIZE + 64, 0);

        let json = br#"{"backend":"hyperv","test":"t"}"#;
        assert!(matches!(
            patch_opentmk_config(&mut image, json),
            Err(PatchError::Truncated(_))
        ));
    }

    #[test]
    fn stray_magic_outside_section_ignored() {
        // A copy of the magic outside `.tmkcfg` (as the compiler materializes in
        // unoptimized builds, e.g. in `.rdata`) must not defeat patching.
        let mut image = image_with_region();
        image.extend_from_slice(&OPENTMK_CONFIG_MAGIC);
        let json = br#"{"backend":"hyperv","test":"hv_processor"}"#;
        patch_opentmk_config(&mut image, json).unwrap();

        let (base, _) = config_section_range(&image).unwrap();
        let json_len = u32::from_le_bytes(
            image[base + OFFSET_JSON_LEN..base + OFFSET_JSON_LEN + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let start = base + OFFSET_JSON;
        let cfg: TestConfig = serde_json::from_slice(&image[start..start + json_len]).unwrap();
        assert_eq!(cfg.backend, "hyperv");
        assert_eq!(cfg.test, "hv_processor");
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
