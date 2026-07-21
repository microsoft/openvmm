// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for resolving `IgvmPlatformType` <-> compatibility-mask
//! mappings declared in an IGVM file's platform headers, and for
//! deriving human-readable / file-name-safe short names for each
//! supported platform.

use igvm::IgvmPlatformHeader;
use igvm_defs::IgvmPlatformType;

/// Short lowercase name for a measurable platform, suitable for file
/// names and CLI suffixes (e.g. `"snp"`, `"tdx"`, `"vbs"`).
///
/// Returns `None` for platform variants that don't have a canonical
/// short name in this tool (e.g. `Native`).
pub fn isolation_short_name(platform: IgvmPlatformType) -> Option<&'static str> {
    match platform {
        IgvmPlatformType::SEV_SNP => Some("snp"),
        IgvmPlatformType::TDX => Some("tdx"),
        IgvmPlatformType::VSM_ISOLATION => Some("vbs"),
        _ => None,
    }
}

/// Always-safe label for a platform, used to generate sibling file
/// names and human-readable platform tags. Falls back to
/// `platform_<Debug>` for variants without a canonical short name,
/// ensuring two distinct platforms never collide on the same label.
pub fn isolation_label(platform: IgvmPlatformType) -> String {
    match isolation_short_name(platform) {
        Some(name) => name.to_string(),
        None => format!("platform_{platform:?}"),
    }
}

/// Look up the compatibility mask for a given platform type by reading the
/// platform headers from the IGVM file.
///
/// Each IGVM file declares its own platform-to-mask mapping via
/// `IGVM_VHS_SUPPORTED_PLATFORM` headers.
///
/// Returns an error if the requested platform type is not present in the
/// file's platform headers.
pub fn lookup_compatibility_mask(
    platforms: &[IgvmPlatformHeader],
    platform: IgvmPlatformType,
) -> anyhow::Result<u32> {
    for header in platforms {
        match header {
            IgvmPlatformHeader::SupportedPlatform(info) => {
                if info.platform_type == platform {
                    return Ok(info.compatibility_mask);
                }
            }
        }
    }

    anyhow::bail!(
        "Platform type {platform:?} not found in IGVM file platform headers. \
         Available platforms: {}",
        platforms
            .iter()
            .map(|h| match h {
                IgvmPlatformHeader::SupportedPlatform(info) => {
                    format!(
                        "{:?} (mask=0x{:X})",
                        info.platform_type, info.compatibility_mask
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}
