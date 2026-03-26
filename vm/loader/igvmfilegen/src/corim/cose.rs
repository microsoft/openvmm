// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CBOR and COSE_Sign1 parsing and manipulation utilities for CoRIM.
//!
//! This module provides a minimal, zero-copy CBOR parser and COSE_Sign1
//! operations needed for handling Concise Reference Integrity Manifest
//! (CoRIM) payloads. The CBOR helpers operate on raw byte slices without
//! re-encoding, which is critical for preserving the exact byte
//! representation of protected headers used in cryptographic signatures.
//!
//! # Design rationale
//!
//! These utilities are intentionally implemented without an external CBOR
//! crate (e.g., `ciborium`) because:
//!
//! - COSE_Sign1 signatures cover the *exact byte encoding* of the
//!   protected headers. Any re-encoding (even semantically equivalent)
//!   would invalidate the signature.
//! - The operations needed (skip, split, validate) are simple positional
//!   manipulations, not full decode-mutate-encode round-trips.
//! - Avoiding external dependencies aligns with the project's policy of
//!   minimizing binary size and third-party risk.
//!
//! # Future work
//!
//! This module is a candidate for extraction into a standalone
//! `support/corim` crate once a second consumer appears (e.g., local
//! verification for TDISP devices). The future crate would also include
//! CoMID (Concise Module Identifier) parsing and COSE_Sign1 signature
//! verification. The scope is intentionally limited to the CoRIM format
//! since that is the only endorsement format we currently support.

use anyhow::Context;
use open_enum::open_enum;

open_enum! {
    /// CBOR major type (3-bit value in bits 7–5 of the initial byte).
    /// See RFC 8949 Section 3.1 for details.
    pub enum CborMajorType: u8 {
        /// Unsigned integer (major type 0).
        UNSIGNED_INT = 0,
        /// Negative integer (major type 1).
        NEGATIVE_INT = 1,
        /// Byte string (major type 2).
        BYTE_STRING = 2,
        /// Text string (major type 3).
        TEXT_STRING = 3,
        /// Array (major type 4).
        ARRAY = 4,
        /// Map (major type 5).
        MAP = 5,
        /// Tag (major type 6).
        TAG = 6,
        /// Simple value or float (major type 7).
        SIMPLE = 7,
    }
}

open_enum! {
    /// CBOR additional info encoding (low 5 bits of the initial byte)
    /// for multi-byte argument lengths.
    /// See RFC 8949 Section 3 for details.
    pub enum CborAdditionalInfo: u8 {
        /// 1-byte unsigned integer argument follows.
        ONE_BYTE = 24,
        /// 2-byte unsigned integer argument follows.
        TWO_BYTE = 25,
        /// 4-byte unsigned integer argument follows.
        FOUR_BYTE = 26,
    }
}

/// CBOR simple value nil (major type 7, additional info 22) — encoded as `0xF6`.
/// See RFC 8949 Appendix A for examples of value encodings.
const CBOR_NIL: u8 = 0xF6;

/// Canonical single-byte encoding of CBOR Tag(18) for COSE_Sign1: `0xD2`.
///
/// CBOR tags use major type 6 (bits 7–5 = `0b110`). For tag values
/// 0–23 the value fits in the 5-bit additional info field, so the
/// entire tag is one byte: `0xC0 | value`.
///
/// Tag(18) → `0xC0 | 0x12` = `0xD2`.
///
/// This is the *preferred serialization* per RFC 8949 Section 4.1
/// ("deterministic encoding"). We enforce this canonical form and
/// reject the non-preferred two-byte encoding `[0xD8, 18]`.
///
/// See also RFC 9052 Section 4.2 for the COSE_Sign1 structure definition.
const COSE_SIGN1_TAG: u8 = 0xD2;

/// Number of elements in a COSE_Sign1 array.
const COSE_SIGN1_ARRAY_LEN: u32 = 4;

/// Mask to extract the additional info field (low 5 bits) from a CBOR
/// initial byte.
const CBOR_ADDITIONAL_INFO_MASK: u8 = 0x1F;

/// Parsed structural offsets within a COSE_Sign1 message.
///
/// Returned by [`parse_cose_sign1_prefix`] after validating the common
/// prefix shared by all COSE_Sign1 operations.
struct CoseSign1Layout {
    /// Whether the canonical CBOR Tag(18) prefix (`0xD2`) was present.
    has_tag: bool,
    /// Byte offset of element \[2\] (payload) within the input slice.
    payload_offset: usize,
}

/// Parse the common prefix of a COSE_Sign1 message.
///
/// Validates and skips:
/// 1. Optional CBOR Tag(18) in canonical form (`0xD2`)
/// 2. 4-element CBOR array header
/// 3. Element [0] — protected headers (must be a bstr)
/// 4. Element [1] — unprotected headers (must be a map)
///
/// Only the canonical single-byte Tag(18) encoding (`0xD2`) is accepted.
/// The non-preferred two-byte form (`0xD8, 0x12`) is rejected per
/// RFC 8949 Section 4.1 (preferred serialization).
///
/// Returns a [`CoseSign1Layout`] with offsets so the caller can inspect
/// element [2] (payload) and beyond.
fn parse_cose_sign1_prefix(data: &[u8]) -> anyhow::Result<CoseSign1Layout> {
    if data.is_empty() {
        anyhow::bail!("COSE_Sign1 data is empty");
    }

    let mut off: usize = 0;

    // Optional CBOR Tag(18) — only the canonical single-byte encoding
    // (0xD2) is accepted. The tag is optional per RFC 9052 §4.2.
    let has_tag = if data[off] == COSE_SIGN1_TAG {
        off += 1;
        true
    } else {
        false
    };

    // Array header — must be a 4-element CBOR array.
    if off >= data.len() {
        anyhow::bail!("truncated before array header");
    }
    let initial = data[off];
    let major = CborMajorType(initial >> 5);
    if major != CborMajorType::ARRAY {
        anyhow::bail!("expected CBOR array (major type 4), got major type {major:?}");
    }
    let additional_info = initial & CBOR_ADDITIONAL_INFO_MASK;
    let (array_len, mut off) = cbor_decode_argument(data, off + 1, additional_info)?;
    if array_len != COSE_SIGN1_ARRAY_LEN {
        anyhow::bail!("COSE_Sign1 array must have 4 elements, got {array_len}");
    }

    // [0] protected headers — must be a bstr (major type 2).
    if off >= data.len() {
        anyhow::bail!("truncated before protected header");
    }
    if CborMajorType(data[off] >> 5) != CborMajorType::BYTE_STRING {
        anyhow::bail!(
            "element [0] (protected) must be a bstr, got major type {:?}",
            CborMajorType(data[off] >> 5)
        );
    }
    off = cbor_skip_item(data, off)?;

    // [1] unprotected headers — must be a map (major type 5).
    if off >= data.len() {
        anyhow::bail!("truncated before unprotected header");
    }
    if CborMajorType(data[off] >> 5) != CborMajorType::MAP {
        anyhow::bail!(
            "element [1] (unprotected) must be a map, got major type {:?}",
            CborMajorType(data[off] >> 5)
        );
    }
    off = cbor_skip_item(data, off)?;

    // Ensure there's data for the payload element.
    if off >= data.len() {
        anyhow::bail!("truncated before payload");
    }

    Ok(CoseSign1Layout {
        has_tag,
        payload_offset: off,
    })
}

/// Decode a CBOR additional info field to get the argument value and advance
/// past any extra length bytes.
///
/// CBOR encodes lengths and small integers in the "additional info" field
/// (the low 5 bits of the initial byte). Values 0–23 are stored directly;
/// values 24–26 indicate 1-, 2-, or 4-byte unsigned integers follow.
///
/// Returns `(argument_value, new_offset)` or an error if the data is
/// truncated.
pub(crate) fn cbor_decode_argument(
    data: &[u8],
    offset: usize,
    additional_info: u8,
) -> anyhow::Result<(u32, usize)> {
    if additional_info < CborAdditionalInfo::ONE_BYTE.0 {
        Ok((additional_info as u32, offset))
    } else if additional_info == CborAdditionalInfo::ONE_BYTE.0 {
        if offset >= data.len() {
            anyhow::bail!("CBOR truncated: expected 1-byte argument at offset {offset}");
        }
        Ok((data[offset] as u32, offset + 1))
    } else if additional_info == CborAdditionalInfo::TWO_BYTE.0 {
        if offset + 2 > data.len() {
            anyhow::bail!("CBOR truncated: expected 2-byte argument at offset {offset}");
        }
        let val = u16::from_be_bytes([data[offset], data[offset + 1]]);
        Ok((val as u32, offset + 2))
    } else if additional_info == CborAdditionalInfo::FOUR_BYTE.0 {
        if offset + 4 > data.len() {
            anyhow::bail!("CBOR truncated: expected 4-byte argument at offset {offset}");
        }
        let val = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        Ok((val, offset + 4))
    } else {
        anyhow::bail!("Unsupported CBOR additional info value {additional_info}");
    }
}

/// Skip one complete CBOR data item starting at `offset`.
///
/// Walks the CBOR encoding to find the end of the current item without
/// materializing its value. Handles all major types including nested
/// arrays, maps, and tagged items.
///
/// Returns the offset immediately after the item, or an error if the
/// encoding is invalid or the data is truncated.
pub(crate) fn cbor_skip_item(data: &[u8], offset: usize) -> anyhow::Result<usize> {
    if offset >= data.len() {
        anyhow::bail!("CBOR truncated at offset {offset}");
    }

    let initial = data[offset];
    let major = CborMajorType(initial >> 5);
    let additional_info = initial & CBOR_ADDITIONAL_INFO_MASK;
    let (arg, mut off) = cbor_decode_argument(data, offset + 1, additional_info)?;

    match major {
        CborMajorType::UNSIGNED_INT | CborMajorType::NEGATIVE_INT => Ok(off),
        CborMajorType::BYTE_STRING | CborMajorType::TEXT_STRING => {
            let Some(end) = off.checked_add(arg as usize) else {
                anyhow::bail!(
                    "CBOR string length {arg} at offset {offset} overflows usize when added to offset"
                );
            };
            if end > data.len() {
                anyhow::bail!("CBOR string length {arg} exceeds data at offset {offset}");
            }
            Ok(end)
        }
        CborMajorType::ARRAY => {
            for _ in 0..arg {
                off = cbor_skip_item(data, off)?;
            }
            Ok(off)
        }
        CborMajorType::MAP => {
            for _ in 0..arg {
                off = cbor_skip_item(data, off)?; // key
                off = cbor_skip_item(data, off)?; // value
            }
            Ok(off)
        }
        CborMajorType::TAG => cbor_skip_item(data, off),
        CborMajorType::SIMPLE => Ok(off),
        _ => anyhow::bail!("Unknown CBOR major type {major:?}"),
    }
}

/// Split a bundled (payload-embedded) COSE_Sign1 into its CoRIM document
/// payload and a detached COSE_Sign1 signature.
///
/// A signed CoRIM is `Tag(18) [ protected, unprotected, payload, signature ]`
/// where `payload` is a `bstr` containing the CBOR-encoded CoRIM document.
///
/// This function:
/// 1. Parses the COSE_Sign1 to locate the embedded payload bstr
/// 2. Extracts the raw payload bytes → returned as the document
/// 3. Rebuilds the COSE_Sign1 with the payload replaced by nil (0xF6)
///    → returned as the detached signature
///
/// The replacement preserves all other bytes verbatim (tag prefix,
/// protected headers, unprotected headers, signature), ensuring the
/// cryptographic signature remains valid for verification against the
/// detached payload.
///
/// # Returns
/// `(corim_document, detached_signature)` — both as `Vec<u8>`.
///
/// # Errors
/// Returns an error if the input is not a valid COSE_Sign1 with an
/// embedded bstr payload.
pub fn split_cose_sign1(data: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let layout = parse_cose_sign1_prefix(data).context("Signed CoRIM")?;
    let payload_start = layout.payload_offset;

    // [2] payload — must be a bstr (embedded), not nil
    if data[payload_start] == CBOR_NIL {
        anyhow::bail!(
            "Signed CoRIM: payload is nil (already detached). \
             Use --corim-document and --corim-signature instead"
        );
    }
    if CborMajorType(data[payload_start] >> 5) != CborMajorType::BYTE_STRING {
        anyhow::bail!(
            "Signed CoRIM: element [2] (payload) must be a bstr, got major type {:?}",
            CborMajorType(data[payload_start] >> 5)
        );
    }

    // Decode the bstr header to find the payload content
    let payload_additional = data[payload_start] & CBOR_ADDITIONAL_INFO_MASK;
    let (payload_len, payload_content_start) =
        cbor_decode_argument(data, payload_start + 1, payload_additional)?;
    let payload_content_end = payload_content_start + payload_len as usize;
    if payload_content_end > data.len() {
        anyhow::bail!("Signed CoRIM: payload bstr extends beyond end of data");
    }

    // Extract the document payload
    let document = data[payload_content_start..payload_content_end].to_vec();

    // The rest after payload is [3] signature + any trailing bytes
    let after_payload = payload_content_end;

    // Build the detached signature by replacing the payload bstr with nil (0xF6)
    let mut detached_sig = Vec::with_capacity(data.len());
    // Copy everything before the payload (tag prefix + array header + elem0 + elem1)
    detached_sig.extend_from_slice(&data[..payload_start]);
    // Replace payload with nil
    detached_sig.push(CBOR_NIL);
    // Copy everything after the payload (elem3 signature + any trailing)
    detached_sig.extend_from_slice(&data[after_payload..]);

    tracing::info!(
        input_size = data.len(),
        document_size = document.len(),
        detached_signature_size = detached_sig.len(),
        had_tag = layout.has_tag,
        "Split signed CoRIM into document payload and detached COSE_Sign1 signature"
    );

    Ok((document, detached_sig))
}

/// Validate that `data` is a well-formed COSE_Sign1 structure with a nil
/// (detached) payload.
///
/// COSE_Sign1 = Tag(18) \[ protected : bstr, unprotected : map, payload :
/// bstr / nil, signature : bstr \]
///
/// The function checks:
/// 1. Optional CBOR Tag(18) prefix (canonical `0xD2` only)
/// 2. 4-element CBOR array
/// 3. Element 0 is a bstr (protected headers)
/// 4. Element 1 is a map (unprotected headers)
/// 5. Element 2 is nil (0xF6) — i.e. payload is detached
/// 6. Element 3 is a bstr (signature)
///
/// TODO: should live in the igvm crate
pub fn validate_cose_sign1_nil_payload(data: &[u8]) -> anyhow::Result<()> {
    let layout = parse_cose_sign1_prefix(data).context("CoRIM signature")?;
    let mut off = layout.payload_offset;

    // [2] payload — must be nil (0xF6) for detached signature
    if data[off] != CBOR_NIL {
        if CborMajorType(data[off] >> 5) == CborMajorType::BYTE_STRING {
            anyhow::bail!(
                "CoRIM signature: payload is an embedded bstr, but must be nil (detached). \
                 Use `igvmfilegen patch-corim --corim-bundle ...` or the `split_cose_sign1` helper \
                 to extract the payload and create a detached signature"
            );
        }
        anyhow::bail!(
            "CoRIM signature: element [2] (payload) must be nil (0xF6), got 0x{:02X}",
            data[off]
        );
    }
    off += 1;

    // [3] signature — must be a bstr (major type 2)
    if off >= data.len() {
        anyhow::bail!("CoRIM signature truncated before signature element");
    }
    if CborMajorType(data[off] >> 5) != CborMajorType::BYTE_STRING {
        anyhow::bail!(
            "CoRIM signature: element [3] (signature) must be a bstr, got major type {:?}",
            CborMajorType(data[off] >> 5)
        );
    }
    off = cbor_skip_item(data, off)?;

    // Verify we consumed all (or nearly all) of the data
    if off > data.len() {
        anyhow::bail!("CoRIM signature CBOR extends beyond end of data");
    }

    tracing::info!(
        size = data.len(),
        "CoRIM signature validated: well-formed COSE_Sign1 with nil payload"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_cose_sign1_nil_payload_valid() {
        // Minimal COSE_Sign1: Tag(18) [ h'' (empty protected), {} (empty map), nil, h'' (empty sig) ]
        // 0xD2 = Tag(18)
        // 0x84 = Array(4)
        // 0x40 = bstr(0)        — empty protected headers
        // 0xA0 = map(0)         — empty unprotected headers
        // 0xF6 = nil            — detached payload
        // 0x40 = bstr(0)        — empty signature
        let valid = [0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x40];
        assert!(validate_cose_sign1_nil_payload(&valid).is_ok());
    }

    #[test]
    fn test_validate_cose_sign1_nil_payload_no_tag() {
        // Valid COSE_Sign1 without Tag(18) prefix — tag is optional per
        // RFC 9052 §4.2.
        let valid_no_tag = [0x84, 0x40, 0xA0, 0xF6, 0x40];
        assert!(validate_cose_sign1_nil_payload(&valid_no_tag).is_ok());
    }

    #[test]
    fn test_validate_cose_sign1_two_byte_tag_rejected() {
        // Two-byte Tag(18) encoding [0xD8, 0x12] is valid CBOR but not
        // canonical (RFC 8949 §4.1). We reject it.
        let two_byte_tag = [0xD8, 0x12, 0x84, 0x40, 0xA0, 0xF6, 0x40];
        let err = validate_cose_sign1_nil_payload(&two_byte_tag).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("CBOR array") || msg.contains("major type"),
            "Should fail parsing because 0xD8 is not recognized as a tag: {msg}"
        );
    }

    #[test]
    fn test_validate_cose_sign1_embedded_payload_rejected() {
        // COSE_Sign1 with embedded payload (bstr instead of nil)
        // 0x84 = Array(4)
        // 0x40 = bstr(0)
        // 0xA0 = map(0)
        // 0x43 = bstr(3) with 3 bytes — NOT nil
        // 0x40 = bstr(0)
        let embedded = [0x84, 0x40, 0xA0, 0x43, 0x01, 0x02, 0x03, 0x40];
        let err = validate_cose_sign1_nil_payload(&embedded).unwrap_err();
        assert!(
            err.to_string().contains("nil"),
            "Error should mention nil: {err}"
        );
    }

    #[test]
    fn test_validate_cose_sign1_wrong_array_length() {
        // Array(3) instead of Array(4)
        let wrong_len = [0xD2, 0x83, 0x40, 0xA0, 0xF6];
        let err = validate_cose_sign1_nil_payload(&wrong_len).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("4 elements"),
            "Error should mention 4 elements: {msg}"
        );
    }

    #[test]
    fn test_validate_cose_sign1_empty() {
        let err = validate_cose_sign1_nil_payload(&[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "Error should mention empty: {msg}");
    }

    #[test]
    fn test_validate_cose_sign1_non_empty_unprotected_map() {
        // COSE_Sign1 with a non-empty unprotected map: { 1: h'FF' }
        // 0xD2 = Tag(18)
        // 0x84 = Array(4)
        // 0x40 = bstr(0)        — empty protected
        // 0xA1 = map(1)         — 1 key-value pair
        //   0x01 = uint(1)      — key
        //   0x41 0xFF = bstr(1) — value
        // 0xF6 = nil            — detached payload
        // 0x40 = bstr(0)        — empty signature
        let valid = [0xD2, 0x84, 0x40, 0xA1, 0x01, 0x41, 0xFF, 0xF6, 0x40];
        assert!(validate_cose_sign1_nil_payload(&valid).is_ok());
    }

    #[test]
    fn test_split_cose_sign1_basic() {
        // Build a COSE_Sign1 with an embedded 3-byte payload: h'AABBCC'
        // Tag(18) [ bstr(0), map(0), bstr(3){AA BB CC}, bstr(2){DE AD} ]
        let signed: Vec<u8> = vec![
            0xD2, // Tag(18)
            0x84, // Array(4)
            0x40, // bstr(0) — empty protected
            0xA0, // map(0)  — empty unprotected
            0x43, 0xAA, 0xBB, 0xCC, // bstr(3) — embedded payload
            0x42, 0xDE, 0xAD, // bstr(2) — signature
        ];

        let (doc, detached) = split_cose_sign1(&signed).unwrap();

        // Document should be the raw payload bytes
        assert_eq!(doc, vec![0xAA, 0xBB, 0xCC]);

        // Detached signature should have nil in place of the payload
        let expected_detached: Vec<u8> = vec![
            0xD2, // Tag(18)
            0x84, // Array(4)
            0x40, // bstr(0) — empty protected
            0xA0, // map(0)  — empty unprotected
            0xF6, // nil     — detached payload
            0x42, 0xDE, 0xAD, // bstr(2) — signature
        ];
        assert_eq!(detached, expected_detached);

        // The detached signature should pass nil-payload validation
        assert!(validate_cose_sign1_nil_payload(&detached).is_ok());
    }

    #[test]
    fn test_split_cose_sign1_no_tag() {
        // COSE_Sign1 without Tag(18) prefix — tag is optional per
        // RFC 9052 §4.2.
        let signed: Vec<u8> = vec![
            0x84, // Array(4)
            0x40, // bstr(0)
            0xA0, // map(0)
            0x42, 0x01, 0x02, // bstr(2) — payload
            0x41, 0xFF, // bstr(1) — signature
        ];

        let (doc, detached) = split_cose_sign1(&signed).unwrap();
        assert_eq!(doc, vec![0x01, 0x02]);

        let expected_detached: Vec<u8> = vec![
            0x84, // Array(4)
            0x40, // bstr(0)
            0xA0, // map(0)
            0xF6, // nil
            0x41, 0xFF, // bstr(1) — signature
        ];
        assert_eq!(detached, expected_detached);
        assert!(validate_cose_sign1_nil_payload(&detached).is_ok());
    }

    #[test]
    fn test_split_cose_sign1_two_byte_tag_rejected() {
        // Two-byte Tag(18) encoding [0xD8, 0x12] is rejected — only
        // canonical 0xD2 is accepted.
        let signed: Vec<u8> = vec![
            0xD8, 0x12, // Two-byte Tag(18) — non-canonical
            0x84, // Array(4)
            0x40, // bstr(0)
            0xA0, // map(0)
            0x42, 0x01, 0x02, // bstr(2) — payload
            0x41, 0xFF, // bstr(1) — signature
        ];
        assert!(split_cose_sign1(&signed).is_err());
    }

    #[test]
    fn test_split_cose_sign1_with_non_empty_headers() {
        // COSE_Sign1 with non-empty protected and unprotected headers
        // protected: bstr wrapping map { 1: -7 }  → 0x43 0xA1 0x01 0x26
        // unprotected: map { 4: h'3131' }          → 0xA1 0x04 0x42 0x31 0x31
        let signed: Vec<u8> = vec![
            0xD2, // Tag(18)
            0x84, // Array(4)
            0x43, 0xA1, 0x01, 0x26, // bstr(3) protected = { 1: -7 }
            0xA1, 0x04, 0x42, 0x31, 0x31, // map(1) unprotected = { 4: h'3131' }
            0x44, 0xCA, 0xFE, 0xBA, 0xBE, // bstr(4) payload
            0x48, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // bstr(8) signature
        ];

        let (doc, detached) = split_cose_sign1(&signed).unwrap();
        assert_eq!(doc, vec![0xCA, 0xFE, 0xBA, 0xBE]);

        // Verify the detached signature structure
        let expected_detached: Vec<u8> = vec![
            0xD2, // Tag(18)
            0x84, // Array(4)
            0x43, 0xA1, 0x01, 0x26, // protected (unchanged)
            0xA1, 0x04, 0x42, 0x31, 0x31, // unprotected (unchanged)
            0xF6, // nil (was payload)
            0x48, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // signature (unchanged)
        ];
        assert_eq!(detached, expected_detached);
        assert!(validate_cose_sign1_nil_payload(&detached).is_ok());
    }

    #[test]
    fn test_split_cose_sign1_already_detached_errors() {
        // A COSE_Sign1 that already has nil payload should fail
        let detached = vec![0xD2, 0x84, 0x40, 0xA0, 0xF6, 0x40];
        let err = split_cose_sign1(&detached).unwrap_err();
        assert!(
            err.to_string().contains("already detached"),
            "Error should mention already detached: {err}"
        );
    }

    #[test]
    fn test_split_cose_sign1_empty_errors() {
        let err = split_cose_sign1(&[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "Error should mention empty: {msg}");
    }

    #[test]
    fn test_split_cose_sign1_round_trip() {
        // Verify that splitting and then validating works for a larger payload
        // Simulate a 256-byte payload (uses 2-byte length encoding: 0x59 0x01 0x00)
        let payload: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        let signature: Vec<u8> = vec![0xAB; 64]; // 64-byte signature

        let mut signed = vec![
            0xD2, // Tag(18)
            0x84, // Array(4)
            0x40, // bstr(0) protected
            0xA0, // map(0) unprotected
            // bstr(256) = 0x59 0x01 0x00 (2-byte length)
            0x59, 0x01, 0x00,
        ];
        signed.extend_from_slice(&payload);
        // bstr(64) = 0x58 0x40 (1-byte length)
        signed.extend_from_slice(&[0x58, 0x40]);
        signed.extend_from_slice(&signature);

        let (doc, detached) = split_cose_sign1(&signed).unwrap();
        assert_eq!(doc, payload);
        assert!(validate_cose_sign1_nil_payload(&detached).is_ok());

        // Verify the signature bytes are preserved in the detached version
        // The detached version ends with: 0x58 0x40 <64 bytes>
        let sig_start = detached.len() - 64;
        assert_eq!(&detached[sig_start..], &signature[..]);
    }
}
