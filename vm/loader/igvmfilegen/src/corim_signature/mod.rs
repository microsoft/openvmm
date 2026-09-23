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
//! - [`envelope`] -- operations on signed CoRIM envelopes (split a bundled
//!   envelope into document + detached signature; cryptographically verify
//!   a detached signature using the issuer cert from its `x5chain` /
//!   `x5bag` header).
//! - [`patch`] -- verify a CoRIM signature against the document
//!   already embedded in an IGVM file, then patch (or replace) the
//!   corresponding `CorimSignature` header.

mod envelope;

#[cfg(test)]
mod test_helpers;

// Re-export signed-CoRIM operations for use by main.rs and other consumers.
pub use envelope::detach_payload;

use anyhow::Context;
use igvm::IgvmFile;
use igvm::IgvmSerializer;
use igvm_defs::IgvmPlatformType;

/// Verify a CoRIM signature against the document already embedded in an
/// IGVM file, then patch the corresponding `CorimSignature` header.
///
/// The CoRIM document is expected to already be present in the IGVM file
/// for the target platform (auto-generated at build time). This function:
///
/// 1. Parses the IGVM file and locates the existing `CorimDocument` for
///    the target platform.
/// 2. If `expected_document` is provided, asserts that it matches the
///    in-file document byte-for-byte. This catches the common UX trap
///    where a user supplies a `--corim-bundle` whose embedded payload
///    was signed against a different document than the one baked into
///    the IGVM file: without this check the failure would surface as an
///    opaque "cryptographic verification failed" error.
/// 3. Cryptographically verifies `corim_signature` against the in-file
///    document via [`envelope::verify_corim_signature`]. The issuer
///    certificate is taken from the signature envelope's `x5chain` /
///    `x5bag` header.
/// 4. Stages the signature replacement via
///    [`IgvmSerializer::set_corim_signature`], which suppresses any
///    in-file document/signature pair for the platform and re-emits the
///    existing document followed by the new signature in the required
///    order, then serializes.
///
/// Verification runs before any serializer mutation, so a failed
/// verification leaves no partially-modified output.
///
/// # Arguments
/// * `igvm_data` - The original IGVM file contents
/// * `corim_signature` - Detached COSE_Sign1 signature payload (nil payload)
/// * `platform` - The target platform type
/// * `expected_document` - Optional CoRIM document bytes that the caller
///   asserts should match the document embedded in the IGVM file. Used
///   when the signature was extracted from a bundled envelope; pass
///   `None` when the caller doesn't have an independent copy.
///
/// # Returns
/// The modified IGVM file contents with the CoRIM signature header
/// inserted or updated.
///
/// # Errors
/// Returns an error if the IGVM file has no `CorimDocument` header for
/// the target platform -- the signature cannot be attached without a
/// corresponding document -- or if `expected_document` is provided and
/// does not match the in-file document, or if cryptographic verification
/// of `corim_signature` against the in-file document fails.
pub fn patch(
    igvm_data: &[u8],
    corim_signature: &[u8],
    platform: IgvmPlatformType,
    expected_document: Option<&[u8]>,
) -> anyhow::Result<Vec<u8>> {
    // Parse the IGVM file using the igvm crate's structured API.
    let igvm_file =
        IgvmFile::new_from_binary(igvm_data, None).context("parsing input IGVM file")?;

    // Validate the target platform is present, so an unknown platform gives a
    // clear "not found" error distinct from the "no document" case below.
    crate::platform_mask::lookup_compatibility_mask(igvm_file.platforms(), platform)?;

    // The serializer exposes the effective CoRIM document for a platform
    // (`corim_for`), so we no longer scan initialization headers ourselves.
    let mut serializer = IgvmSerializer::new(&igvm_file).context("constructing IGVM serializer")?;

    // Verify the signature against the in-file document *before* any serializer
    // mutation, so a failed verification leaves no partially-modified output.
    // The immutable borrow of `existing_doc` is scoped to this block so the
    // subsequent `&mut serializer` call to `set_corim_signature` is allowed.
    {
        let existing_doc = serializer.corim_for(platform).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot patch CoRIM signature for platform {platform:?}: no CoRIM \
                 document present in the IGVM file. The document must be embedded \
                 at IGVM generation time before a signature can be attached."
            )
        })?;

        // If the caller supplied the document they think the signature was
        // produced against (typically extracted from a bundled COSE_Sign1
        // envelope), check that it matches the in-file document. This produces
        // a targeted error before the cryptographic verify path would
        // otherwise fail with an opaque "verification failed" message.
        if let Some(expected) = expected_document
            && expected != existing_doc
        {
            anyhow::bail!(
                "CoRIM document mismatch for platform {platform:?}: the document \
                 carried by the input bundle ({} bytes) does not byte-match the \
                 document embedded in the IGVM file ({} bytes). The bundle was \
                 signed against a different document; re-sign against the \
                 IGVM-embedded document or supply only the detached signature \
                 via `--corim-signature`.",
                expected.len(),
                existing_doc.len(),
            );
        }

        // The issuer certificate is taken from the envelope's protected header
        // (x5chain / x5bag).
        envelope::verify_corim_signature(corim_signature, existing_doc)
            .context("verifying CoRIM signature against the in-file document")?;
    }

    // Stage the signature replacement on the serializer. `set_corim_signature`
    // suppresses the in-file document/signature pair for this platform and
    // re-emits the existing document followed by the new signature, keeping
    // the required document-before-signature ordering without us having to
    // reconstruct the full IgvmFile.
    serializer
        .set_corim_signature(platform, corim_signature.to_vec())
        .context("staging CoRIM signature replacement")?;

    // Serialize back to binary. The igvm crate handles file offsets,
    // alignment, and CRC32 checksum.
    let mut output = Vec::new();
    serializer
        .serialize(&mut output)
        .context("serializing patched IGVM file")?;

    tracing::info!(
        original_size = igvm_data.len(),
        new_size = output.len(),
        signature_size = corim_signature.len(),
        platform = ?platform,
        "Patched CoRIM signature into IGVM file",
    );

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corim_signature::test_helpers::sign_envelope_for;
    use igvm::IgvmDirectiveHeader;
    use igvm::IgvmInitializationHeader;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmRevision;
    use igvm::IgvmSerializer;
    use igvm::corim::launch_measurement::LaunchMeasurement;
    use igvm::corim::launch_measurement::MeasurementKind;
    use igvm_defs::IGVM_FIXED_HEADER;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

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

    /// Build `GuestPolicy` initialization headers for every SEV-SNP
    /// platform present. `IgvmSerializer::new` eagerly measures all
    /// measurable platforms, and SNP measurement requires a `GuestPolicy`
    /// header -- real manifest-built files always carry one, so the test
    /// helpers synthesize an equivalent here.
    fn snp_guest_policies(platforms: &[IgvmPlatformHeader]) -> Vec<IgvmInitializationHeader> {
        platforms
            .iter()
            .filter_map(|p| match p {
                IgvmPlatformHeader::SupportedPlatform(info)
                    if info.platform_type == IgvmPlatformType::SEV_SNP =>
                {
                    Some(IgvmInitializationHeader::GuestPolicy {
                        policy: 0x30000,
                        compatibility_mask: info.compatibility_mask,
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// Build a minimal IGVM binary from given headers (no CoRIM).
    fn build_igvm(
        platforms: Vec<IgvmPlatformHeader>,
        directives: Vec<IgvmDirectiveHeader>,
    ) -> Vec<u8> {
        let initializations = snp_guest_policies(&platforms);
        let igvm = IgvmFile::new(IgvmRevision::V1, platforms, initializations, directives)
            .expect("valid IgvmFile");
        let mut output = Vec::new();
        igvm.serialize(&mut output).expect("serialize");
        output
    }

    /// Build a minimal IGVM binary with pre-embedded CoRIM document(s).
    /// `documents` is a list of `(compatibility_mask, document_bytes)`
    /// pairs. The order of resulting `CorimDocument` initializations
    /// matches the list order.
    fn build_igvm_with_corim_docs(
        platforms: Vec<IgvmPlatformHeader>,
        directives: Vec<IgvmDirectiveHeader>,
        documents: Vec<(u32, Vec<u8>)>,
    ) -> Vec<u8> {
        let mut initializations: Vec<IgvmInitializationHeader> = snp_guest_policies(&platforms);
        initializations.extend(documents.into_iter().map(|(mask, doc)| {
            IgvmInitializationHeader::CorimDocument {
                compatibility_mask: mask,
                document: doc,
            }
        }));
        let igvm = IgvmFile::new(IgvmRevision::V1, platforms, initializations, directives)
            .expect("valid IgvmFile");
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
    fn test_patch_corim_add_signature() {
        let page_data = vec![0xCC; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
            vec![(0x1, b"corim-payload".to_vec())],
        );

        let sig = sign_envelope_for(b"corim-payload", "test");
        let patched = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("patch should succeed");

        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, b"corim-payload");
        assert_eq!(sigs[0].payload, sig);
        // Document and signature must share the same mask.
        assert_eq!(docs[0].compatibility_mask, sigs[0].compatibility_mask);
    }

    #[test]
    fn test_patch_corim_preserves_non_corim_directives() {
        let data1 = vec![0x11; 4096];
        let data2 = vec![0x22; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &data1), new_page_data(1, 0x1, &data2)],
            vec![(0x1, b"doc".to_vec())],
        );

        let original_count = count_non_corim_directive_headers(&igvm_data);

        let sig = sign_envelope_for(b"doc", "test");
        let patched = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("patch should succeed");

        let patched_count = count_non_corim_directive_headers(&patched);
        assert_eq!(original_count, patched_count);
    }

    #[test]
    fn test_patch_corim_preserves_platform_headers() {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
            vec![(0x1, b"vbs-corim".to_vec())],
        );

        let original_platforms = extract_platform_types(&igvm_data);

        let sig = sign_envelope_for(b"vbs-corim", "test");
        let patched = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("patch should succeed");

        let patched_platforms = extract_platform_types(&patched);
        assert_eq!(original_platforms, patched_platforms);
    }

    #[test]
    fn test_patch_corim_uses_correct_mask() {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
            vec![(0x1, b"vbs-corim".to_vec()), (0x2, b"snp-corim".to_vec())],
        );

        // Patch signature for VBS only.
        let sig = sign_envelope_for(b"vbs-corim", "test");
        let patched = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("patch should succeed");

        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 2, "both platform docs preserved");
        assert_eq!(sigs.len(), 1, "only VBS signature added");
        assert_eq!(sigs[0].compatibility_mask, 0x1);
    }

    #[test]
    fn test_patch_corim_error_platform_not_in_file() {
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &vec![0; 4096])],
            vec![(0x1, b"doc".to_vec())],
        );

        // Platform-lookup failure fires before signature verification,
        // so the envelope contents are irrelevant here.
        let sig = sign_envelope_for(b"doc", "test");
        let result = patch(
            &igvm_data,
            &sig,
            IgvmPlatformType::SEV_SNP, // Not in file
            None,
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found' error, got: {msg}"
        );
    }

    #[test]
    fn test_patch_corim_error_missing_document() {
        // Signature-only patching on a file without an existing document
        // for the target platform must fail with a targeted error.
        let igvm_data = build_igvm(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &vec![0; 4096])],
        );

        // Missing-document failure fires before signature verification.
        let sig = sign_envelope_for(b"doc", "test");
        let err = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no CoRIM document"),
            "expected 'no CoRIM document' error, got: {msg}"
        );
    }

    #[test]
    fn test_patch_corim_output_is_valid_igvm_header() {
        let page_data = vec![0x77; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
            vec![(0x1, b"round-trip-doc".to_vec())],
        );

        let sig = sign_envelope_for(b"round-trip-doc", "test");
        let patched = patch(&igvm_data, &sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("patch should succeed");

        let fixed = IGVM_FIXED_HEADER::read_from_prefix(&patched)
            .expect("valid fixed header")
            .0;
        assert_eq!(fixed.magic, igvm_defs::IGVM_MAGIC_VALUE);
        assert_eq!(fixed.format_version, 1);
        assert_eq!(fixed.total_file_size as usize, patched.len());

        // `IgvmFile::new_from_binary` recomputes and verifies the CRC32
        // over the variable header section. A successful re-parse here
        // confirms the patched file's CRC32 was correctly recomputed.
        IgvmFile::new_from_binary(&patched, None)
            .expect("patched file must pass IGVM CRC32 validation");
    }

    #[test]
    fn test_patch_corim_bundle_document_mismatch() {
        // When the caller supplies a bundle whose payload differs from
        // the document embedded in the IGVM file, `patch` must surface a
        // targeted mismatch error before attempting cryptographic
        // verification (which would otherwise fail with an opaque
        // \"verification failed\" message).
        let page_data = vec![0xAB; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
            vec![(0x1, b"in-file-doc".to_vec())],
        );

        // Build a syntactically valid signature; the mismatch check must
        // fire before envelope::verify_corim_signature is reached.
        let sig = sign_envelope_for(b"in-file-doc", "test");

        let err = patch(
            &igvm_data,
            &sig,
            IgvmPlatformType::VSM_ISOLATION,
            Some(b"different-bundled-doc"),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not byte-match"),
            "expected bundle/in-file mismatch error, got: {msg}"
        );
    }

    #[test]
    fn test_patch_corim_round_trip_reparse() {
        // Verify that a patched file can be re-parsed and re-patched
        // (round-trip through new_from_binary works after the igvm crate
        // CoRIM parsing fix).
        let page_data = vec![0xDD; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
            vec![(0x1, b"first-doc".to_vec())],
        );

        let first_sig = sign_envelope_for(b"first-doc", "test");
        let patched = patch(
            &igvm_data,
            &first_sig,
            IgvmPlatformType::VSM_ISOLATION,
            None,
        )
        .expect("first patch should succeed");

        // Re-patching with a different signature must also succeed and
        // must preserve the document.
        let second_sig = sign_envelope_for(b"first-doc", "test-alt");
        let repatched = patch(&patched, &second_sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("re-patching should succeed");

        let (docs, sigs) = extract_corim_headers(&repatched);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, b"first-doc");
        assert_eq!(sigs[0].payload, second_sig);
    }

    #[test]
    fn test_patch_corim_replace_signature_preserves_document() {
        let page_data = vec![0xFF; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![new_platform(0x1, IgvmPlatformType::VSM_ISOLATION)],
            vec![new_page_data(0, 0x1, &page_data)],
            vec![(0x1, b"keep-this-doc".to_vec())],
        );

        // First: attach an initial signature.
        let first_sig = sign_envelope_for(b"keep-this-doc", "test");
        let with_sig = patch(
            &igvm_data,
            &first_sig,
            IgvmPlatformType::VSM_ISOLATION,
            None,
        )
        .expect("initial signature attach");

        // Replace it with a different signature.
        let second_sig = sign_envelope_for(b"keep-this-doc", "test-alt");
        let updated = patch(
            &with_sig,
            &second_sig,
            IgvmPlatformType::VSM_ISOLATION,
            None,
        )
        .expect("signature replacement");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 1);
        assert_eq!(sigs.len(), 1);
        assert_eq!(docs[0].payload, b"keep-this-doc");
        assert_eq!(sigs[0].payload, second_sig);
    }

    /// Helper: build a multi-platform IGVM file with CoRIM documents AND
    /// signatures already attached for both VBS (mask=0x1) and SNP
    /// (mask=0x2). Returns the file along with the VBS and SNP signatures
    /// so callers can use them in assertions.
    fn build_multi_platform_with_corim() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let data = vec![0x55; 4096];
        let igvm_data = build_igvm_with_corim_docs(
            vec![
                new_platform(0x1, IgvmPlatformType::VSM_ISOLATION),
                new_platform(0x2, IgvmPlatformType::SEV_SNP),
            ],
            vec![new_page_data(0, 0x1, &data), new_page_data(0, 0x2, &data)],
            vec![(0x1, b"vbs-doc".to_vec()), (0x2, b"snp-doc".to_vec())],
        );

        // Attach signature to VBS.
        let vbs_sig = sign_envelope_for(b"vbs-doc", "test");
        let with_vbs = patch(&igvm_data, &vbs_sig, IgvmPlatformType::VSM_ISOLATION, None)
            .expect("VBS signature attach");

        // Attach signature to SNP.
        let snp_sig = sign_envelope_for(b"snp-doc", "test");
        let with_both = patch(&with_vbs, &snp_sig, IgvmPlatformType::SEV_SNP, None)
            .expect("SNP signature attach");

        (with_both, vbs_sig, snp_sig)
    }

    #[test]
    fn test_multi_platform_corim_interleaved_ordering_is_valid() {
        let (with_both, _vbs_sig, _snp_sig) = build_multi_platform_with_corim();

        let (docs, sigs) = extract_corim_headers(&with_both);
        assert_eq!(docs.len(), 2, "should have docs for both platforms");
        assert_eq!(sigs.len(), 2, "should have sigs for both platforms");

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
    fn test_multi_platform_replace_signature_preserves_other_platform() {
        // Replace SNP signature while VBS CoRIM is also present.
        // VBS headers must be completely unchanged.
        let (with_both, vbs_sig, _snp_sig) = build_multi_platform_with_corim();

        let new_snp_sig = sign_envelope_for(b"snp-doc", "test-alt");
        let updated = patch(&with_both, &new_snp_sig, IgvmPlatformType::SEV_SNP, None)
            .expect("update SNP signature");

        let (docs, sigs) = extract_corim_headers(&updated);
        assert_eq!(docs.len(), 2);
        assert_eq!(sigs.len(), 2);

        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig_after = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig_after = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc", "VBS doc must be unchanged");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc preserved");
        assert_eq!(vbs_sig_after.payload, vbs_sig, "VBS sig must be unchanged");
        assert_eq!(
            snp_sig_after.payload, new_snp_sig,
            "SNP sig must be the new one"
        );

        IgvmFile::new_from_binary(&updated, None).expect("output should be valid IGVM");
    }

    #[test]
    fn test_multi_platform_sequential_updates_both_platforms() {
        // Update VBS first, then SNP. Verify both updates are reflected
        // and the file remains valid after each step.
        let (with_both, _vbs_sig, _snp_sig) = build_multi_platform_with_corim();

        // Step 1: replace VBS signature.
        let new_vbs_sig = sign_envelope_for(b"vbs-doc", "test-alt");
        let after_vbs = patch(
            &with_both,
            &new_vbs_sig,
            IgvmPlatformType::VSM_ISOLATION,
            None,
        )
        .expect("VBS update");

        IgvmFile::new_from_binary(&after_vbs, None).expect("valid after VBS update");

        // Step 2: replace SNP signature.
        let new_snp_sig = sign_envelope_for(b"snp-doc", "test-alt");
        let after_snp =
            patch(&after_vbs, &new_snp_sig, IgvmPlatformType::SEV_SNP, None).expect("SNP update");

        let (docs, sigs) = extract_corim_headers(&after_snp);
        assert_eq!(docs.len(), 2);
        assert_eq!(sigs.len(), 2);

        let vbs_doc = docs.iter().find(|d| d.compatibility_mask == 0x1).unwrap();
        let snp_doc = docs.iter().find(|d| d.compatibility_mask == 0x2).unwrap();
        let vbs_sig = sigs.iter().find(|s| s.compatibility_mask == 0x1).unwrap();
        let snp_sig = sigs.iter().find(|s| s.compatibility_mask == 0x2).unwrap();

        assert_eq!(vbs_doc.payload, b"vbs-doc", "VBS doc preserved");
        assert_eq!(snp_doc.payload, b"snp-doc", "SNP doc preserved");
        assert_eq!(vbs_sig.payload, new_vbs_sig, "VBS sig from step 1");
        assert_eq!(snp_sig.payload, new_snp_sig, "SNP sig from step 2");

        IgvmFile::new_from_binary(&after_snp, None).expect("valid after both updates");
    }

    /// End-to-end exercise of the production pipeline: build a real IGVM
    /// file, attach a real CoRIM document via `IgvmSerializer::add_corim`
    /// (the same call site `create_igvm_file` uses), then sign the
    /// resulting CoRIM document with PS384 and patch the signature in via
    /// `patch`. Verifies that the patched file still
    /// parses, the original document survives unchanged, and the patched
    /// signature matches what was produced from the real CoRIM bytes.
    #[test]
    fn test_e2e_real_corim_build_and_patch() {
        let platform = IgvmPlatformType::VSM_ISOLATION;
        let mask = 0x1;

        // Build a minimal valid IGVM file with one platform and one page.
        let page_data = vec![0xAA; 4096];
        let base = build_igvm(
            vec![new_platform(mask, platform)],
            vec![new_page_data(0, mask, &page_data)],
        );

        // Attach a real CoRIM launch endorsement, exactly as the
        // production `create_igvm_file` post-merge step does.
        let parsed = IgvmFile::new_from_binary(&base, None).expect("parse base IGVM");
        let mut serializer = IgvmSerializer::new(&parsed).expect("construct serializer");
        let mut le = LaunchMeasurement::for_platform(platform).expect("launch endorsement");
        le.set_measurement(MeasurementKind::Launch)
            .expect("set measurement kind");
        le.endorse(1)
            .with(MeasurementKind::Launch)
            .expect("CES with")
            .finish()
            .expect("CES finish");
        let real_corim = serializer
            .add_corim(platform, le.build())
            .expect("add_corim")
            .to_vec();

        let mut with_doc = Vec::new();
        serializer.serialize(&mut with_doc).expect("serialize");

        // The serializer must have embedded exactly the CoRIM bytes it
        // returned, and no signature should be present yet.
        let (docs, sigs) = extract_corim_headers(&with_doc);
        assert_eq!(docs.len(), 1, "one CoRIM document embedded");
        assert!(sigs.is_empty(), "no signature before patch");
        assert_eq!(
            docs[0].payload, real_corim,
            "embedded doc matches add_corim return"
        );

        // Sign the real CoRIM document with PS384 and patch the signature
        // into the IGVM file.
        let signature = sign_envelope_for(&real_corim, "e2e-test");
        let patched = patch(&with_doc, &signature, platform, None).expect("patch signature");

        // The patched file must still parse, preserve the real document,
        // and now carry exactly the signature we produced.
        IgvmFile::new_from_binary(&patched, None).expect("patched file parses");
        let (docs, sigs) = extract_corim_headers(&patched);
        assert_eq!(docs.len(), 1, "one CoRIM document after patch");
        assert_eq!(sigs.len(), 1, "one CoRIM signature after patch");
        assert_eq!(docs[0].compatibility_mask, mask);
        assert_eq!(sigs[0].compatibility_mask, mask);
        assert_eq!(docs[0].payload, real_corim, "real CoRIM doc preserved");
        assert_eq!(sigs[0].payload, signature, "real signature attached");
    }
}
