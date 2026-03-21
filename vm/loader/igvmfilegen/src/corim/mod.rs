// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for patching CoRIM (Concise Reference Integrity Manifest) headers
//! into an existing IGVM file.
//!
//! CoRIM headers allow embedding signed measurement payloads that can be
//! verified by the platform's attestation infrastructure.
//!
//! # Module structure
//!
//! - [`cose`] — CBOR and COSE_Sign1 parsing/manipulation utilities.
//!   These are format-level operations with no IGVM dependency and are
//!   candidates for extraction into a standalone `support/corim` crate
//!   once a second consumer exists (e.g., OpenHCL paravisor attestation).

mod cose;

// Re-export COSE_Sign1 operations for use by main.rs and other consumers.
pub use cose::split_cose_sign1;
pub use cose::validate_cose_sign1_nil_payload;

use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use igvm::IgvmRevision;
use igvm_defs::IGVM_FIXED_HEADER;
use igvm_defs::IgvmPlatformType;
use zerocopy::FromBytes;

/// Determine the [`IgvmRevision`] from raw IGVM binary data by inspecting the
/// fixed header's `format_version` field.
///
/// Currently only IGVM format version 1 is supported. V2 support would
/// require enabling the `unstable` feature on `igvm_defs`.
fn igvm_revision_from_binary(data: &[u8]) -> anyhow::Result<IgvmRevision> {
    let (header, _) = IGVM_FIXED_HEADER::read_from_prefix(data)
        .map_err(|_| anyhow::anyhow!("Invalid IGVM file: cannot read fixed header"))?;

    // TODO: Support V2 when CoRIM is required for AArch64.
    match header.format_version {
        1 => Ok(IgvmRevision::V1),
        other => anyhow::bail!(
            "Unsupported IGVM format version {other} (only V1 is supported for CoRIM patching)"
        ),
    }
}

/// Patch CoRIM headers into an existing IGVM file.
///
/// This function uses the `igvm` crate's structured API to parse the IGVM
/// file, modify the directive headers, and re-serialize. This delegates all
/// offset management, alignment, and checksum calculation to the `igvm`
/// crate.
///
/// If a CoRIM document or signature already exists for the target platform,
/// it will be replaced with the new data. This ensures there is at most one
/// CoRIM document and one CoRIM signature per platform.
///
/// # Arguments
/// * `igvm_data` - The original IGVM file contents
/// * `corim_document` - Optional CoRIM CBOR document payload
/// * `corim_signature` - Optional COSE_Sign1 signature payload
/// * `platform` - The target platform type
///
/// At least one of `corim_document` or `corim_signature` must be provided.
///
/// # Returns
/// The modified IGVM file contents with CoRIM headers inserted or updated
pub fn patch_corim(
    igvm_data: &[u8],
    corim_document: Option<&[u8]>,
    corim_signature: Option<&[u8]>,
    platform: IgvmPlatformType,
) -> anyhow::Result<Vec<u8>> {
    // Validate that at least one of document or signature is provided
    if corim_document.is_none() && corim_signature.is_none() {
        anyhow::bail!("At least one of corim_document or corim_signature must be provided");
    }

    // Validate the COSE_Sign1 signature structure if provided
    if let Some(sig) = corim_signature {
        validate_cose_sign1_nil_payload(sig)?;
    }

    // Determine the IGVM revision from the fixed header (needed to
    // reconstruct the file after modifying directives).
    let revision = igvm_revision_from_binary(igvm_data)?;

    // Parse the IGVM file using the igvm crate's structured API.
    // No isolation filter — we want all headers so we can selectively
    // replace only the CoRIM directives for our target platform.
    let igvm_file = IgvmFile::new_from_binary(igvm_data, None)
        .map_err(|e| anyhow::anyhow!("Failed to parse IGVM file: {e}"))?;

    // Look up the compatibility mask for the requested platform from the
    // file's actual platform headers, rather than assuming a hardcoded mapping.
    let compatibility_mask = crate::lookup_compatibility_mask(igvm_file.platforms(), platform)?;

    // Build new directive headers:
    // - Keep all non-CoRIM directives unchanged
    // - Keep CoRIM directives for other platforms unchanged
    // - Drop ALL CoRIM directives for our target platform (both document
    //   and signature), preserving their data so we can re-append any
    //   that the caller did not provide a replacement for. This ensures
    //   the (document, signature) pair is always emitted in the correct
    //   order at the end, even when only one is being updated.
    let mut new_directives: Vec<IgvmDirectiveHeader> = Vec::new();
    let mut existing_doc: Option<Vec<u8>> = None;
    let mut existing_sig: Option<Vec<u8>> = None;

    for header in igvm_file.directives() {
        match header {
            IgvmDirectiveHeader::CorimDocument {
                compatibility_mask: mask,
                document,
            } if *mask == compatibility_mask => {
                tracing::info!(
                    compatibility_mask = format_args!("0x{mask:X}"),
                    "Removing existing CoRIM document header"
                );
                existing_doc = Some(document.clone());
            }
            IgvmDirectiveHeader::CorimSignature {
                compatibility_mask: mask,
                signature,
            } if *mask == compatibility_mask => {
                tracing::info!(
                    compatibility_mask = format_args!("0x{mask:X}"),
                    "Removing existing CoRIM signature header"
                );
                existing_sig = Some(signature.clone());
            }
            other => {
                new_directives.push(other.clone());
            }
        }
    }

    // Determine final document and signature: prefer caller-provided data,
    // fall back to the existing data we preserved above.
    let had_existing = existing_doc.is_some() || existing_sig.is_some();
    let final_doc = corim_document.map(|d| d.to_vec()).or(existing_doc);
    let final_sig = corim_signature.map(|s| s.to_vec()).or(existing_sig);

    // Append CoRIM headers in the required order (document before signature).
    if let Some(doc) = final_doc {
        new_directives.push(IgvmDirectiveHeader::CorimDocument {
            compatibility_mask,
            document: doc,
        });
    }
    if let Some(sig) = final_sig {
        new_directives.push(IgvmDirectiveHeader::CorimSignature {
            compatibility_mask,
            signature: sig,
        });
    }

    // Reconstruct the IGVM file with modified directives.
    // IgvmFile::new() validates the header structure (e.g., at most one
    // CoRIM document/signature per compatibility mask, document before
    // signature).
    let new_igvm = IgvmFile::new(
        revision,
        igvm_file.platforms().to_vec(),
        igvm_file.initializations().to_vec(),
        new_directives,
    )
    .map_err(|e| anyhow::anyhow!("Failed to construct IGVM file with new CoRIM headers: {e}"))?;

    // Serialize back to binary. The igvm crate handles file offsets,
    // alignment, and CRC32 checksum.
    let mut output = Vec::new();
    new_igvm
        .serialize(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to serialize IGVM file: {e}"))?;

    let action = if had_existing { "Updated" } else { "Added" };

    tracing::info!(
        action = action,
        original_size = igvm_data.len(),
        new_size = output.len(),
        document_size = corim_document.map(|d| d.len()).unwrap_or(0),
        signature_size = corim_signature.map(|s| s.len()).unwrap_or(0),
        platform = ?platform,
        "{} CoRIM headers in IGVM file",
        action
    );

    Ok(output)
}
