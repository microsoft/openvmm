// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for building the UEFI images an OpenHCL VM's GED can load in
//! response to a `LoadFirmware` (firmware overload) request.

use anyhow::Context;
use get_resources::ged::UefiOverloadImage;
use std::path::Path;

/// Returns the hibernate firmware token a mu_msvm UEFI image identifies
/// itself with: its `major.minor`, with major in the high byte.
///
/// Returns `None` if the image has no version record.
pub fn uefi_firmware_token(image: &[u8]) -> anyhow::Result<Option<u64>> {
    let Some(version) = loader::uefi::firmware_version::find_in_firmware_image(image)
        .context("invalid uefi firmware version record")?
    else {
        return Ok(None);
    };
    token_from_base_version(version.base_version)
        .with_context(|| format!("unrecognized uefi version {:?}", version.base_version))
        .map(Some)
}

/// Builds the GED overload image that loads `image` for `token`.
pub fn uefi_overload_image(token: u64, image: Vec<u8>) -> anyhow::Result<UefiOverloadImage> {
    let layout = loader::uefi::image_layout(&image).context("invalid uefi image")?;
    Ok(UefiOverloadImage {
        token,
        base_gpa: layout.base_gpa,
        entry_offset: layout.entry_offset,
        image,
    })
}

/// The legacy `LoadFirmware` token of OpenHCL 1.8, from before tokens tracked
/// the UEFI version.
pub const OPENHCL_1_8_TOKEN: u64 = 0x0108;

/// Reads the UEFI image at `path` and builds the GED overload catalog it
/// serves: the token it identifies itself with (if it has a version record)
/// and [`OPENHCL_1_8_TOKEN`].
pub fn uefi_overload_images_from_file(path: &Path) -> anyhow::Result<Vec<UefiOverloadImage>> {
    let image = fs_err::read(path)?;
    let mut tokens = Vec::new();
    if let Some(token) = uefi_firmware_token(&image)? {
        tokens.push(token);
    }
    // OpenHCL 1.8 shipped a 26.0 UEFI, which this image supersedes.
    tokens.push(OPENHCL_1_8_TOKEN);
    tokens
        .into_iter()
        .map(|token| uefi_overload_image(token, image.clone()))
        .collect()
}

fn token_from_base_version(base_version: &str) -> Option<u64> {
    let mut parts = base_version.split('.');
    let major: u8 = parts.next()?.parse().ok()?;
    let minor: u8 = parts.next()?.parse().ok()?;
    Some((u64::from(major) << 8) | u64::from(minor))
}

#[cfg(test)]
mod tests {
    use super::token_from_base_version;
    use super::uefi_overload_image;

    #[test]
    fn overload_image_rejects_wrong_size() {
        assert!(uefi_overload_image(0x1A00, vec![0; 0x1000]).is_err());
    }

    #[test]
    fn base_version_to_token() {
        assert_eq!(token_from_base_version("26.0.44"), Some(0x1A00));
        assert_eq!(token_from_base_version("26.1"), Some(0x1A01));
        assert_eq!(token_from_base_version("26"), None);
        assert_eq!(token_from_base_version("v26.0.44"), None);
        assert_eq!(token_from_base_version("256.0.0"), None);
    }
}
