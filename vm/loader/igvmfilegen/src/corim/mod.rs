// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for patching CoRIM (Concise Reference Integrity Manifest) headers
//! into an existing IGVM file.
//!
//! CoRIM headers allow embedding signed endorsement for the IGVM file that
//! can be verified by the attestation service.
//!
//! # Module structure
//!
//! - [`cose`] — CBOR and COSE_Sign1 parsing/manipulation utilities.
//!   These are format-level operations with no IGVM dependency and are
//!   candidates for extraction into a standalone `support/corim` crate
//!   once a second consumer exists (e.g., local verification for TDISP devices).

mod cose;

// Re-export COSE_Sign1 operations for use by main.rs and other consumers.
pub use cose::split_cose_sign1;
pub use cose::validate_cose_sign1_nil_payload;

use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
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
    // file's actual platform headers.
    let compatibility_mask = crate::lookup_compatibility_mask(igvm_file.platforms(), platform)?;

    // Build new initialization headers:
    // - Keep all non-CoRIM initialization headers unchanged
    // - Keep CoRIM headers for other platforms unchanged
    // - Drop all CoRIM headers for our target platform (both document
    //   and signature), preserving their data so we can re-append any
    //   that the caller did not provide a replacement for. This ensures
    //   the (document, signature) pair is always emitted in the correct
    //   order at the end, even when only one is being updated.
    let mut new_initializations: Vec<IgvmInitializationHeader> = Vec::new();
    let mut existing_doc: Option<Vec<u8>> = None;
    let mut existing_sig: Option<Vec<u8>> = None;

    for header in igvm_file.initializations() {
        match header {
            IgvmInitializationHeader::CorimDocument {
                compatibility_mask: mask,
                document,
            } if *mask == compatibility_mask => {
                tracing::info!(
                    compatibility_mask = format_args!("0x{mask:X}"),
                    "Removing existing CoRIM document header"
                );
                existing_doc = Some(document.clone());
            }
            IgvmInitializationHeader::CorimSignature {
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
                new_initializations.push(other.clone());
            }
        }
    }

    // Determine final document and signature: prefer caller-provided data,
    // fall back to the existing data we preserved above.
    let had_existing = existing_doc.is_some() || existing_sig.is_some();
    let final_doc = corim_document.map(|d| d.to_vec()).or(existing_doc.clone());
    let final_sig = corim_signature.map(|s| s.to_vec()).or(existing_sig.clone());

    // Log when existing headers are being preserved (caller did not provide
    // a replacement). This avoids confusion when the final output includes
    // data the caller didn't explicitly pass on the command line.
    if corim_document.is_none() && existing_doc.is_some() {
        tracing::info!("Preserving existing CoRIM document from input file");
    }
    if corim_signature.is_none() && existing_sig.is_some() {
        tracing::info!("Preserving existing CoRIM signature from input file");
    }

    // If a signature is requested but there is no corresponding document,
    // fail early with a targeted error message rather than relying on the
    // generic validation error from IgvmFile::new().
    if final_sig.is_some() && final_doc.is_none() {
        anyhow::bail!(
            "Cannot attach CoRIM signature for compatibility mask 0x{compatibility_mask:X} \
             without a corresponding CoRIM document. Provide --corim-document or ensure an \
             existing document is present for this mask before adding a signature."
        );
    }
    // Capture final sizes for logging (before values are moved).
    let final_doc_size = final_doc.as_ref().map(|d| d.len()).unwrap_or(0);
    let final_sig_size = final_sig.as_ref().map(|s| s.len()).unwrap_or(0);

    // Append CoRIM headers in the required order (document before signature).
    if let Some(document) = final_doc {
        new_initializations.push(IgvmInitializationHeader::CorimDocument {
            compatibility_mask,
            document,
        });
    }
    if let Some(signature) = final_sig {
        new_initializations.push(IgvmInitializationHeader::CorimSignature {
            compatibility_mask,
            signature,
        });
    }

    // Reconstruct the IGVM file with modified initialization headers.
    // IgvmFile::new() validates the header structure (e.g., at most one
    // CoRIM document/signature per compatibility mask, document before
    // signature).
    let new_igvm = IgvmFile::new(
        revision,
        igvm_file.platforms().to_vec(),
        new_initializations,
        igvm_file.directives().to_vec(),
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
        final_document_size = final_doc_size,
        final_signature_size = final_sig_size,
        platform = ?platform,
        "{} CoRIM headers in IGVM file",
        action
    );

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use igvm::IgvmDirectiveHeader;
    use igvm::IgvmPlatformHeader;
    use igvm_defs::IGVM_FIXED_HEADER;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use test_with_tracing::test;

    /// Minimal COSE_Sign1 with nil payload (detached signature).
    const COSE_SIGN1_NIL: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x40];

    fn new_platform(
        compatibility_mask: u32,
        platform_type: IgvmPlatformType,
    ) -> IgvmPlatformHeader {
        IgvmPlatformHeader::SupportedPlatform(IGVM_VHS_SUPPORTED_PLATFORM {
            compatibility_mask,
            highest_vtl: 0,
            platform_type,
            platform_version: 1,
            shared_gpa_boundary: 0,
        })
    }

    fn new_page_data(page: u64, compatibility_mask: u32, data: &[u8]) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::PageData {
            gpa: page * 4096,
            compatibility_mask,
            flags: IgvmPageDataFlags::new(),
            data_type: IgvmPageDataType::NORMAL,
            data: data.to_vec(),
        }
    }

    /// Build a minimal IGVM binary from given headers.
    fn build_igvm(
        platforms: Vec<IgvmPlatformHeader>,
        directives: Vec<IgvmDirectiveHeader>,
    ) -> Vec<u8> {
        let igvm =
            IgvmFile::new(IgvmRevision::V1, platforms, vec![], directives).expect("valid IgvmFile");
        let mut output = Vec::new();
        igvm.serialize(&mut output).expect("serialize");
        output
    }

    /// Extracted CoRIM header info for test assertions.
    struct CorimHeaderInfo {
        compatibility_mask: u32,
        payload: Vec<u8>,
    }

    /// Parse an IGVM binary and extract CoRIM document and signature headers
    /// using the structured `IgvmFile` API.
    fn extract_corim_headers(data: &[u8]) -> (Vec<CorimHeaderInfo>, Vec<CorimHeaderInfo>) {
        let igvm = IgvmFile::new_from_binary(data, None).expect("valid IGVM file");
        let mut documents = Vec::new();
        let mut signatures = Vec::new();

        for header in igvm.initializations() {
            match header {
                IgvmInitializationHeader::CorimDocument {
                    compatibility_mask,
                    document,
                } => {
                    documents.push(CorimHeaderInfo {
                        compatibility_mask: *compatibility_mask,
                        payload: document.clone(),
                    });
                }
                IgvmInitializationHeader::CorimSignature {
                    compatibility_mask,
                    signature,
                } => {
                    signatures.push(CorimHeaderInfo {
                        compatibility_mask: *compatibility_mask,
                        payload: signature.clone(),
                    });
                }
                _ => {}
            }
        }

        (documents, signatures)
    }

    /// Count directive headers (excluding CoRIM) in the IGVM binary.
    fn count_non_corim_directive_headers(data: &[u8]) -> usize {
        let igvm = IgvmFile::new_from_binary(data, None).expect("valid IGVM file");
        igvm.directives().len()
    }

    /// Extract platform types and masks from the IGVM binary.
    fn extract_platform_types(data: &[u8]) -> Vec<(IgvmPlatformType, u32)> {
        let igvm = IgvmFile::new_from_binary(data, None).expect("valid IGVM file");
        igvm.platforms()
            .iter()
            .map(|p| match p {
                IgvmPlatformHeader::SupportedPlatform(plat) => {
                    (plat.platform_type, plat.compatibility_mask)
                }
            })
            .collect()
    }

    #[test]
    fn test_patch_corim_add_document_only() {
        let page_data = vec![0xAA; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );
        let document = b"test-corim-document";

        let patched = patch_corim(
            &igvm_data,
            Some(document),
            None,
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        assert!(patched.len() > igvm_data.len());

        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 0);
        assert_eq!(docs[0].payload, document);
        assert_eq!(docs[0].compatibility_mask, 0x1);
    }

    #[test]
    fn test_patch_corim_add_both_document_and_signature() {
        let page_data = vec![0xCC; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );
        let document = b"corim-payload";

        let patched = patch_corim(
            &igvm_data,
            Some(document),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, document);
        assert_eq!(sigs[0].payload, COSE_SIGN1_NIL);

        // Document and signature should share the same mask
        assert_eq!(docs[0].compatibility_mask, sigs[0].compatibility_mask);
    }

    #[test]
    fn test_patch_corim_preserves_non_corim_directives() {
        let data1 = vec![0x11; 4096];
        let data2 = vec![0x22; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &data1), new_page_data(1, 0x1, &data2)],
        );

        let original_count = count_non_corim_directive_headers(&igvm_data);

        let patched = patch_corim(
            &igvm_data,
            Some(b"doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        // Non-CoRIM directive count should be unchanged
        let patched_count = count_non_corim_directive_headers(&patched);
        assert_eq!(original_count, patched_count);
    }

    #[test]
    fn test_patch_corim_preserves_platform_headers() {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
        );

        let original_platforms = extract_platform_types(&igvm_data);

        let patched = patch_corim(
            &igvm_data,
            Some(b"vbs-corim"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        let patched_platforms = extract_platform_types(&patched);
        assert_eq!(original_platforms, patched_platforms);
    }

    #[test]
    fn test_patch_corim_multi_platform_corim_uses_correct_mask() {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
        );

        // Add CoRIM for VBS only
        let patched = patch_corim(
            &igvm_data,
            Some(b"vbs-corim"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        // CoRIM should use VBS mask (0x1), not SNP mask (0x2)
        assert_eq!(docs[0].compatibility_mask, 0x1);
        assert_eq!(sigs[0].compatibility_mask, 0x1);
        assert_eq!(docs[0].payload, b"vbs-corim");
    }

    #[test]
    fn test_patch_corim_error_both_none() {
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &vec![0; 4096])],
        );

        let result = patch_corim(&igvm_data, None, None, IgvmPlatformType::VSM_ISOLATION);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("At least one"),
            "expected 'At least one' error, got: {msg}"
        );
    }

    #[test]
    fn test_patch_corim_error_platform_not_in_file() {
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &vec![0; 4096])],
        );

        let result = patch_corim(
            &igvm_data,
            Some(b"doc"),
            None,
            IgvmPlatformType::SEV_SNP, // Not in file
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found' error, got: {msg}"
        );
    }

    #[test]
    fn test_patch_corim_output_is_valid_igvm_header() {
        let page_data = vec![0x77; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        let patched = patch_corim(
            &igvm_data,
            Some(b"round-trip-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("patch_corim should succeed");

        // Verify the fixed header is valid
        let fixed = IGVM_FIXED_HEADER::read_from_prefix(&patched)
            .expect("valid fixed header")
            .0;
        assert_eq!(fixed.magic, igvm_defs::IGVM_MAGIC_VALUE);
        assert_eq!(fixed.format_version, 1);
        assert_eq!(fixed.total_file_size as usize, patched.len());
    }

    #[test]
    fn test_patch_corim_signature_only_requires_document() {
        // The igvm crate requires a CorimDocument before a CorimSignature
        // for the same compatibility mask, so signature-only patching on a
        // file without an existing document should fail.
        let page_data = vec![0xBB; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        let result = patch_corim(
            &igvm_data,
            None,
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        );
        assert!(
            result.is_err(),
            "signature-only patch without existing document should fail"
        );
    }

    #[test]
    fn test_patch_corim_round_trip_reparse() {
        // Verify that a patched file can be re-parsed and re-patched
        // (round-trip through new_from_binary works after the igvm crate
        // CoRIM parsing fix).
        let page_data = vec![0xDD; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        let patched = patch_corim(
            &igvm_data,
            Some(b"first-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("first patch should succeed");

        // Re-patching should now succeed
        let repatched = patch_corim(
            &patched,
            Some(b"second-doc"),
            None,
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("re-patching should succeed");

        // Verify the document was replaced and signature preserved
        let (docs, sigs) = extract_corim_headers(&repatched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, b"second-doc");
        assert_eq!(sigs[0].payload, COSE_SIGN1_NIL);
    }

    #[test]
    fn test_patch_corim_update_document_preserves_signature() {
        // When updating only the document on a file that already has both
        // document and signature, the existing signature should be preserved.
        let page_data = vec![0xEE; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        // First: add both document and signature
        let with_both = patch_corim(
            &igvm_data,
            Some(b"original-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("initial patch");

        // Second: update only the document
        let updated = patch_corim(
            &with_both,
            Some(b"updated-doc"),
            None,
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("update document only");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, b"updated-doc");
        // Signature should be preserved from the original
        assert_eq!(sigs[0].payload, COSE_SIGN1_NIL);
    }

    #[test]
    fn test_patch_corim_update_signature_preserves_document() {
        // When updating only the signature on a file that already has both
        // document and signature, the existing document should be preserved.
        let page_data = vec![0xFF; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        // First: add both document and signature
        let with_both = patch_corim(
            &igvm_data,
            Some(b"keep-this-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("initial patch");

        // A different valid COSE_Sign1 with nil payload (different empty
        // protected header encoding)
        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];

        // Second: update only the signature
        let updated = patch_corim(
            &with_both,
            None,
            Some(new_sig),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("update signature only");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        // Document should be preserved from the original
        assert_eq!(docs[0].payload, b"keep-this-doc");
        assert_eq!(sigs[0].payload, new_sig);
    }

    #[test]
    fn test_patch_corim_update_both_document_and_signature() {
        // When updating both document and signature on a file that already
        // has existing entries, both should be replaced.
        let page_data = vec![0xAB; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        // First: add original document and signature
        let with_both = patch_corim(
            &igvm_data,
            Some(b"old-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("initial patch");

        // Second: replace both
        let new_doc = b"new-doc-payload";
        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];
        let updated = patch_corim(
            &with_both,
            Some(new_doc),
            Some(new_sig),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("update both");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, new_doc);
        assert_eq!(sigs[0].payload, new_sig);
    }

    #[test]
    fn test_patch_corim_signature_only_preserves_document_round_trip() {
        // End-to-end test: create file with both CoRIM headers, then update
        // only the signature. Verify the document is preserved AND the
        // output is re-parseable (round-trips through new_from_binary).
        let page_data = vec![0xFE; 4096];
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
        );

        let original_doc = b"document-must-survive-signature-only-update";

        // Step 1: add both document and signature
        let with_both = patch_corim(
            &igvm_data,
            Some(original_doc.as_slice()),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("initial patch with both");

        // Step 2: re-parse the output to confirm CoRIM is in initializations
        let parsed =
            IgvmFile::new_from_binary(&with_both, None).expect("output should be parseable");
        let corim_inits: Vec<_> = parsed
            .initializations()
            .iter()
            .filter(|h| {
                matches!(
                    h,
                    IgvmInitializationHeader::CorimDocument { .. }
                        | IgvmInitializationHeader::CorimSignature { .. }
                )
            })
            .collect();
        assert_eq!(
            corim_inits.len(),
            2,
            "parsed file should have 2 CoRIM init headers (doc + sig)"
        );

        // Step 3: update ONLY the signature
        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];
        let updated = patch_corim(
            &with_both,
            None, // no document provided
            Some(new_sig),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("signature-only update should succeed");

        // Step 4: verify via raw binary inspection
        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 1, "document must still be present");
        assert_eq!(sigs.len(), 1, "signature must be present");
        assert_eq!(
            docs[0].payload,
            original_doc.as_slice(),
            "document payload must be identical to the original"
        );
        assert_eq!(
            sigs[0].payload, new_sig,
            "signature must be the newly provided one"
        );

        // Step 5: verify the output is a valid IGVM file (re-parseable)
        let reparsed = IgvmFile::new_from_binary(&updated, None)
            .expect("updated output should be re-parseable");
        let corim_inits_after: Vec<_> = reparsed
            .initializations()
            .iter()
            .filter(|h| {
                matches!(
                    h,
                    IgvmInitializationHeader::CorimDocument { .. }
                        | IgvmInitializationHeader::CorimSignature { .. }
                )
            })
            .collect();
        assert_eq!(
            corim_inits_after.len(),
            2,
            "re-parsed file should still have 2 CoRIM init headers"
        );
    }

    /// Helper: build a multi-platform IGVM file with CoRIM headers for both
    /// VBS (mask=0x1) and SNP (mask=0x2).
    fn build_multi_platform_with_corim() -> Vec<u8> {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
        );

        // Add CoRIM to VBS first
        let with_vbs = patch_corim(
            &igvm_data,
            Some(b"vbs-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("VBS CoRIM patch");

        // Add CoRIM to SNP
        patch_corim(
            &with_vbs,
            Some(b"snp-doc"),
            Some(COSE_SIGN1_NIL),
            IgvmPlatformType::SEV_SNP,
        )
        .expect("SNP CoRIM patch")
    }

    #[test]
    fn test_multi_platform_corim_interleaved_ordering_is_valid() {
        // When CoRIM headers exist for multiple platforms, the initialization
        // header order is: Doc(mask_a), Sig(mask_a), Doc(mask_b), Sig(mask_b).
        // This interleaves type 0x104 and 0x105 across masks. The igvm crate
        // allows this because it validates doc-before-sig per mask, not global
        // type ordering.
        let with_both = build_multi_platform_with_corim();

        // Verify both platform CoRIM pairs are present
        let (docs, sigs) = extract_corim_headers(&with_both);
        assert_eq!(docs.len(), 2, "should have docs for both platforms");
        assert_eq!(sigs.len(), 2, "should have sigs for both platforms");

        // Verify the output is valid and can be re-parsed
        let reparsed = IgvmFile::new_from_binary(&with_both, None)
            .expect("interleaved CoRIM ordering should be parseable");

        let corim_count = reparsed
            .initializations()
            .iter()
            .filter(|h| {
                matches!(
                    h,
                    IgvmInitializationHeader::CorimDocument { .. }
                        | IgvmInitializationHeader::CorimSignature { .. }
                )
            })
            .count();
        assert_eq!(corim_count, 4, "should have 4 CoRIM init headers total");
    }

    #[test]
    fn test_multi_platform_update_document_preserves_other_platform() {
        // Update VBS document while SNP CoRIM is also present.
        // SNP headers must be completely unchanged.
        let with_both = build_multi_platform_with_corim();

        let updated = patch_corim(
            &with_both,
            Some(b"vbs-doc-updated"),
            None,
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("update VBS document");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 2, "both platform docs present");
        assert_eq!(sigs.len(), 2, "both platform sigs present");

        // Find VBS and SNP by mask
        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc-updated");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc must be unchanged");
        assert_eq!(vbs_sig.payload, COSE_SIGN1_NIL, "VBS sig preserved");
        assert_eq!(snp_sig.payload, COSE_SIGN1_NIL, "SNP sig must be unchanged");

        // Must round-trip
        IgvmFile::new_from_binary(&updated, None).expect("output should be valid IGVM");
    }

    #[test]
    fn test_multi_platform_update_signature_preserves_other_platform() {
        // Update SNP signature while VBS CoRIM is also present.
        // VBS headers must be completely unchanged.
        let with_both = build_multi_platform_with_corim();

        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];
        let updated = patch_corim(&with_both, None, Some(new_sig), IgvmPlatformType::SEV_SNP)
            .expect("update SNP signature");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 2);
        assert_eq!(sigs.len(), 2);

        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc", "VBS doc must be unchanged");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc preserved");
        assert_eq!(vbs_sig.payload, COSE_SIGN1_NIL, "VBS sig must be unchanged");
        assert_eq!(snp_sig.payload, new_sig, "SNP sig must be the new one");

        IgvmFile::new_from_binary(&updated, None).expect("output should be valid IGVM");
    }

    #[test]
    fn test_multi_platform_update_both_for_one_platform() {
        // Replace both document and signature for VBS while leaving SNP
        // completely intact.
        let with_both = build_multi_platform_with_corim();

        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];
        let updated = patch_corim(
            &with_both,
            Some(b"vbs-doc-v2"),
            Some(new_sig),
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("update both VBS headers");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 2);
        assert_eq!(sigs.len(), 2);

        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc-v2");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc unchanged");
        assert_eq!(vbs_sig.payload, new_sig);
        assert_eq!(snp_sig.payload, COSE_SIGN1_NIL, "SNP sig unchanged");

        IgvmFile::new_from_binary(&updated, None).expect("output should be valid IGVM");
    }

    #[test]
    fn test_multi_platform_sequential_updates_both_platforms() {
        // Update VBS first, then SNP. Verify both updates are reflected
        // and the file remains valid after each step.
        let with_both = build_multi_platform_with_corim();

        // Step 1: update VBS document
        let after_vbs = patch_corim(
            &with_both,
            Some(b"vbs-doc-step1"),
            None,
            IgvmPlatformType::VSM_ISOLATION,
        )
        .expect("VBS update");

        IgvmFile::new_from_binary(&after_vbs, None).expect("valid after VBS update");

        // Step 2: update SNP signature
        let new_sig: &[u8] = &[0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x41, 0x00];
        let after_snp = patch_corim(&after_vbs, None, Some(new_sig), IgvmPlatformType::SEV_SNP)
            .expect("SNP update");

        let (docs, sigs) = extract_corim_headers(&after_snp);
        assert_eq!(docs.len(), 2);
        assert_eq!(sigs.len(), 2);

        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc-step1", "VBS doc from step 1");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc preserved");
        assert_eq!(vbs_sig.payload, COSE_SIGN1_NIL, "VBS sig preserved");
        assert_eq!(snp_sig.payload, new_sig, "SNP sig from step 2");

        IgvmFile::new_from_binary(&after_snp, None).expect("valid after both updates");
    }
}
