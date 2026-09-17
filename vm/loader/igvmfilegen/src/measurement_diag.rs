// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-platform launch measurement diagnostics.
//!
//! Builds the human-readable launch-measurement structures that downstream
//! signing/attestation tooling expects (`VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA`
//! for VBS, `SnpPspIdBlock` for SEV-SNP, MRTD for TDX) and emits them via
//! `tracing` so they are visible in `igvmfilegen` output. The `igvm` crate
//! itself only computes the raw digest; the diagnostic dressing (svn,
//! debug bit, SNP family/image identifiers, ...) is OpenHCL-specific and
//! lives here.

use bitfield_struct::bitfield;
use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
use igvm_defs::IgvmPlatformType;
use igvm_defs::VbsDigestAlgorithm;
use igvm_defs::VbsSigningAlgorithm;
use x86defs::snp::SnpPspIdBlock;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use crate::snp_id_block::SNP_FAMILY_ID;
use crate::snp_id_block::SNP_IMAGE_ID;

// Name follows the Windows VBS C struct convention; `#[repr(C)]` already
// silences the `non_camel_case_types` lint so no explicit allow is needed.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, Debug)]
struct VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA {
    version: u32,
    product_id: u32,
    module_id: u32,
    security_version: u32,
    security_policy: VBS_POLICY_FLAGS,
    boot_digest_algo: u32,
    signing_algo: u32,
    boot_measurement_digest: [u8; 32],
}

/// Flags defining the security policy for the guest.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout)]
#[expect(non_camel_case_types)]
struct VBS_POLICY_FLAGS {
    /// Guest supports debugging
    #[bits(1)]
    debug: bool,
    #[bits(31)]
    reserved: u32,
}

/// Emit a `tracing` log of the platform-specific launch-measurement
/// diagnostic structure for human inspection.
pub fn log_measurement_diagnostic(
    platform: IgvmPlatformType,
    digest: &[u8],
    svn: u32,
    enable_debug: bool,
    file: &IgvmFile,
    compatibility_mask: u32,
) {
    match platform {
        IgvmPlatformType::VSM_ISOLATION => log_vbs(digest, svn, enable_debug),
        IgvmPlatformType::SEV_SNP => log_snp(digest, svn, file, compatibility_mask),
        IgvmPlatformType::TDX => log_tdx(digest),
        _ => {}
    }
}

fn log_vbs(digest: &[u8], svn: u32, enable_debug: bool) {
    const MSFT_PRODUCT_ID: u32 = u32::from_le_bytes(*b"msft");
    const VBS_MODULE_ID: u32 = u32::from_le_bytes(*b"vbs\0");
    const VBS_VM_BOOT_MEASUREMENT_VERSION_CURRENT: u32 = 0x1;

    // The digest comes from `IgvmSerializer::measurement_for(VSM_ISOLATION)`
    // which contractually returns a 32-byte SHA-256. A length mismatch
    // would indicate a broken in-tree invariant.
    let boot_measurement_digest =
        <[u8; 32]>::try_from(digest).expect("VBS launch digest is 32 bytes");

    let boot_measurement = VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA {
        version: VBS_VM_BOOT_MEASUREMENT_VERSION_CURRENT,
        product_id: MSFT_PRODUCT_ID,
        module_id: VBS_MODULE_ID,
        security_version: svn,
        security_policy: VBS_POLICY_FLAGS::new().with_debug(enable_debug),
        boot_digest_algo: VbsDigestAlgorithm::SHA256.0,
        signing_algo: VbsSigningAlgorithm::ECDSA_P384.0,
        boot_measurement_digest,
    };
    tracing::info!("Boot Measurement {:x?}", boot_measurement);
}

fn log_snp(digest: &[u8], svn: u32, file: &IgvmFile, compatibility_mask: u32) {
    // The digest comes from `IgvmSerializer::measurement_for(SEV_SNP)`
    // which contractually returns a 48-byte SHA-384. A length mismatch
    // would indicate a broken in-tree invariant.
    let ld = <[u8; 48]>::try_from(digest).expect("SNP launch digest is 48 bytes");

    let policy = file
        .initializations()
        .iter()
        .find_map(|h| match h {
            IgvmInitializationHeader::GuestPolicy {
                policy,
                compatibility_mask: mask,
            } if mask & compatibility_mask == compatibility_mask => Some(*policy),
            _ => None,
        })
        .unwrap_or_else(|| {
            tracing::error!(
                compatibility_mask = format_args!("0x{compatibility_mask:X}"),
                "Missing SNP GuestPolicy initialization header; reporting policy as 0"
            );
            0
        });

    let psp_id_block = SnpPspIdBlock {
        ld,
        family_id: SNP_FAMILY_ID,
        image_id: SNP_IMAGE_ID,
        version: 0x1,
        guest_svn: svn,
        policy,
    };
    tracing::info!("SNP ID Block {:x?}", psp_id_block);
}

fn log_tdx(digest: &[u8]) {
    tracing::info!("MRTD: {}", hex::encode_upper(digest));
}
