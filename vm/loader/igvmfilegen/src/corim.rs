// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for patching CoRIM (Concise Reference Integrity Manifest) headers
//! into an existing IGVM file.
//!
//! CoRIM headers allow embedding signed measurement payloads that can be verified
//! by the platform's attestation infrastructure.

use igvm_defs::IGVM_FIXED_HEADER;
use igvm_defs::IGVM_VHS_PAGE_DATA;
use igvm_defs::IGVM_VHS_PARAMETER_AREA;
use igvm_defs::IGVM_VHS_VARIABLE_HEADER;
use igvm_defs::IGVM_VHS_VP_CONTEXT;
use igvm_defs::IgvmPlatformType;
use igvm_defs::IgvmVariableHeaderType;
use std::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// CoRIM measurement header structure (IGVM_VHS_CORIM_DOCUMENT).
///
/// This header references a CBOR CoRIM payload stored in the file data section.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    zerocopy::IntoBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
    zerocopy::FromBytes,
)]
pub struct CorimDocumentHeader {
    /// Compatibility mask for platform filtering.
    pub compatibility_mask: u32,
    /// File offset for the CoRIM CBOR payload.
    pub file_offset: u32,
    /// Size in bytes of the CoRIM CBOR payload.
    pub size_bytes: u32,
    /// Reserved, must be zero.
    pub reserved: u32,
}

/// CoRIM signature header structure (IGVM_VHS_CORIM_SIGNATURE).
///
/// This header references a COSE_Sign1 structure stored in the file data section.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    zerocopy::IntoBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
    zerocopy::FromBytes,
)]
pub struct CorimSignatureHeader {
    /// Compatibility mask for platform filtering.
    pub compatibility_mask: u32,
    /// File offset for the COSE_Sign1 payload.
    pub file_offset: u32,
    /// Size in bytes of the COSE_Sign1 payload.
    pub size_bytes: u32,
    /// Reserved, must be zero.
    pub reserved: u32,
}

/// Get the compatibility mask for a given platform type.
///
/// The compatibility mask is used to filter headers for specific platforms.
/// Currently we use simple bit positions for each platform.
pub fn platform_to_compatibility_mask(platform: IgvmPlatformType) -> u32 {
    // The compatibility mask is typically set based on the platform headers
    // in the IGVM file. For simplicity, we use a 1:1 mapping.
    // In practice, you may want to read the existing platform headers to
    // determine the correct mask.
    match platform {
        IgvmPlatformType::SEV_SNP => 0x1,
        IgvmPlatformType::TDX => 0x2,
        IgvmPlatformType::VSM_ISOLATION => 0x4,
        _ => 0x1, // Default
    }
}

/// Calculate CRC32 checksum for IGVM header validation.
fn calculate_crc32(data: &[u8]) -> u32 {
    // IGVM uses CRC32 (IEEE polynomial)
    let mut crc: u32 = 0xFFFFFFFF;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Information about an existing CoRIM header found during scanning.
#[derive(Debug, Clone)]
struct ExistingCorimHeader {
    /// Position of the variable header entry in the variable headers section.
    var_header_pos: usize,
    /// Total size of the variable header entry (8-byte aligned).
    var_header_entry_size: usize,
    /// File offset of the data in the file data section.
    file_offset: u32,
    /// Size of the data in the file data section.
    data_size: u32,
    /// Whether this is a document (true) or signature (false) header.
    is_document: bool,
}

/// Scan variable headers to find existing CoRIM headers for a specific platform.
fn find_existing_corim_headers(
    var_headers: &[u8],
    target_compatibility_mask: u32,
) -> Vec<ExistingCorimHeader> {
    let mut existing = Vec::new();
    let mut pos = 0;

    while pos + size_of::<IGVM_VHS_VARIABLE_HEADER>() <= var_headers.len() {
        let Ok((header, _)) = IGVM_VHS_VARIABLE_HEADER::read_from_prefix(&var_headers[pos..])
        else {
            break;
        };

        let content_start = pos + size_of::<IGVM_VHS_VARIABLE_HEADER>();
        let content_len = header.length as usize;

        if content_start + content_len > var_headers.len() {
            break;
        }

        let entry_size = align_to_8(size_of::<IGVM_VHS_VARIABLE_HEADER>() + content_len);

        match header.typ {
            IgvmVariableHeaderType::IGVM_VHT_CORIM_DOCUMENT => {
                if content_len >= size_of::<CorimDocumentHeader>() {
                    if let Ok((doc_header, _)) =
                        CorimDocumentHeader::read_from_prefix(&var_headers[content_start..])
                    {
                        if doc_header.compatibility_mask == target_compatibility_mask {
                            existing.push(ExistingCorimHeader {
                                var_header_pos: pos,
                                var_header_entry_size: entry_size,
                                file_offset: doc_header.file_offset,
                                data_size: doc_header.size_bytes,
                                is_document: true,
                            });
                        }
                    }
                }
            }
            IgvmVariableHeaderType::IGVM_VHT_CORIM_SIGNATURE => {
                if content_len >= size_of::<CorimSignatureHeader>() {
                    if let Ok((sig_header, _)) =
                        CorimSignatureHeader::read_from_prefix(&var_headers[content_start..])
                    {
                        if sig_header.compatibility_mask == target_compatibility_mask {
                            existing.push(ExistingCorimHeader {
                                var_header_pos: pos,
                                var_header_entry_size: entry_size,
                                file_offset: sig_header.file_offset,
                                data_size: sig_header.size_bytes,
                                is_document: false,
                            });
                        }
                    }
                }
            }
            _ => {}
        }

        pos += entry_size;
    }

    existing
}

/// Patch CoRIM headers into an existing IGVM file.
///
/// This function supports both adding new CoRIM headers and updating existing ones.
/// If a CoRIM document or signature already exists for the target platform, it will
/// be replaced with the new data. This ensures there is at most one CoRIM document
/// and one CoRIM signature per platform.
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

    // Parse the fixed header
    let (fixed_header, _) = IGVM_FIXED_HEADER::read_from_prefix(igvm_data)
        .map_err(|_| anyhow::anyhow!("Invalid IGVM file: cannot read fixed header"))?;

    // Validate magic value
    const IGVM_MAGIC_VALUE: u32 = 0x4D564749; // "IGVM" in little endian
    if fixed_header.magic != IGVM_MAGIC_VALUE {
        anyhow::bail!(
            "Invalid IGVM magic value: expected {:#x}, got {:#x}",
            IGVM_MAGIC_VALUE,
            fixed_header.magic
        );
    }

    let var_header_offset = fixed_header.variable_header_offset as usize;
    let var_header_size = fixed_header.variable_header_size as usize;
    let original_file_size = fixed_header.total_file_size as usize;

    // Validate file structure
    if igvm_data.len() < original_file_size {
        anyhow::bail!("IGVM file is truncated");
    }

    let var_header_end = var_header_offset + var_header_size;
    let compatibility_mask = platform_to_compatibility_mask(platform);

    // Find existing CoRIM headers for this platform
    let existing_headers = find_existing_corim_headers(
        &igvm_data[var_header_offset..var_header_end],
        compatibility_mask,
    );

    // Separate existing document and signature headers
    let existing_doc = existing_headers.iter().find(|h| h.is_document);
    let existing_sig = existing_headers.iter().find(|h| !h.is_document);

    // Log what we found
    if let Some(doc) = existing_doc {
        tracing::info!(
            "Found existing CoRIM document header at offset {}, data at file offset {}, size {}",
            doc.var_header_pos,
            doc.file_offset,
            doc.data_size
        );
    }
    if let Some(sig) = existing_sig {
        tracing::info!(
            "Found existing CoRIM signature header at offset {}, data at file offset {}, size {}",
            sig.var_header_pos,
            sig.file_offset,
            sig.data_size
        );
    }

    // Build the new file by:
    // 1. Copying non-CoRIM variable headers (filtering out existing CoRIM headers for this platform)
    // 2. Copying non-CoRIM file data (filtering out existing CoRIM data)
    // 3. Adding new CoRIM headers and data

    // Collect positions to skip in variable headers
    let skip_var_header_ranges: Vec<(usize, usize)> = existing_headers
        .iter()
        .filter(|h| {
            // Skip document header if we're providing a new document
            // Skip signature header if we're providing a new signature
            (h.is_document && corim_document.is_some())
                || (!h.is_document && corim_signature.is_some())
        })
        .map(|h| (h.var_header_pos, h.var_header_pos + h.var_header_entry_size))
        .collect();

    // Collect file data ranges to skip
    let skip_file_data_ranges: Vec<(usize, usize)> = existing_headers
        .iter()
        .filter(|h| {
            (h.is_document && corim_document.is_some())
                || (!h.is_document && corim_signature.is_some())
        })
        .map(|h| {
            (
                h.file_offset as usize,
                h.file_offset as usize + h.data_size as usize,
            )
        })
        .collect();

    // Calculate how much variable header space we're removing
    let removed_var_header_size: usize = skip_var_header_ranges.iter().map(|(s, e)| e - s).sum();

    // Calculate sizes for new headers (8-byte aligned)
    let document_entry_size = if corim_document.is_some() {
        align_to_8(size_of::<IGVM_VHS_VARIABLE_HEADER>() + size_of::<CorimDocumentHeader>())
    } else {
        0
    };
    let signature_entry_size = if corim_signature.is_some() {
        align_to_8(size_of::<IGVM_VHS_VARIABLE_HEADER>() + size_of::<CorimSignatureHeader>())
    } else {
        0
    };

    // Net change in variable header size
    let added_var_header_size = document_entry_size + signature_entry_size;
    let new_var_header_size = var_header_size - removed_var_header_size + added_var_header_size;
    let var_header_size_delta = new_var_header_size as isize - var_header_size as isize;

    // Calculate how much file data we're removing
    let removed_file_data_size: usize = skip_file_data_ranges.iter().map(|(s, e)| e - s).sum();

    // Build the output file
    let mut output = Vec::new();

    // 1. Copy fixed header (will be updated later)
    output.extend_from_slice(&igvm_data[..size_of::<IGVM_FIXED_HEADER>()]);

    // 2. Copy any data between fixed header and variable headers
    if var_header_offset > size_of::<IGVM_FIXED_HEADER>() {
        output.extend_from_slice(&igvm_data[size_of::<IGVM_FIXED_HEADER>()..var_header_offset]);
    }

    // 3. Copy existing variable headers, skipping ones we're replacing
    let var_headers_start = output.len();
    copy_with_skips(
        &igvm_data[var_header_offset..var_header_end],
        &skip_var_header_ranges,
        &mut output,
    );
    let var_headers_end_before_new = output.len();

    // 4. Add new CoRIM headers (will fill in file_offset later)
    let new_doc_header_pos = if corim_document.is_some() {
        let pos = output.len();
        let doc_var_header = IGVM_VHS_VARIABLE_HEADER {
            typ: IgvmVariableHeaderType::IGVM_VHT_CORIM_DOCUMENT,
            length: size_of::<CorimDocumentHeader>() as u32,
        };
        output.extend_from_slice(doc_var_header.as_bytes());
        // Placeholder for document header (will be filled in later)
        let placeholder = CorimDocumentHeader {
            compatibility_mask,
            file_offset: 0, // Will be updated
            size_bytes: corim_document.unwrap().len() as u32,
            reserved: 0,
        };
        output.extend_from_slice(placeholder.as_bytes());
        // Pad to 8-byte alignment
        let current_size = size_of::<IGVM_VHS_VARIABLE_HEADER>() + size_of::<CorimDocumentHeader>();
        let padding = document_entry_size - current_size;
        output.extend(std::iter::repeat(0u8).take(padding));
        Some(pos + size_of::<IGVM_VHS_VARIABLE_HEADER>()) // Position of the actual header content
    } else {
        None
    };

    let new_sig_header_pos = if corim_signature.is_some() {
        let pos = output.len();
        let sig_var_header = IGVM_VHS_VARIABLE_HEADER {
            typ: IgvmVariableHeaderType::IGVM_VHT_CORIM_SIGNATURE,
            length: size_of::<CorimSignatureHeader>() as u32,
        };
        output.extend_from_slice(sig_var_header.as_bytes());
        // Placeholder for signature header
        let placeholder = CorimSignatureHeader {
            compatibility_mask,
            file_offset: 0, // Will be updated
            size_bytes: corim_signature.unwrap().len() as u32,
            reserved: 0,
        };
        output.extend_from_slice(placeholder.as_bytes());
        // Pad to 8-byte alignment
        let current_size =
            size_of::<IGVM_VHS_VARIABLE_HEADER>() + size_of::<CorimSignatureHeader>();
        let padding = signature_entry_size - current_size;
        output.extend(std::iter::repeat(0u8).take(padding));
        Some(pos + size_of::<IGVM_VHS_VARIABLE_HEADER>())
    } else {
        None
    };

    // 5. Update file_offset fields in existing variable headers
    // We need to adjust for:
    // - Change in variable header section size (var_header_size_delta)
    // - Removed file data that comes BEFORE each header's data
    update_file_offsets_with_removals(
        &mut output[var_headers_start..var_headers_end_before_new],
        var_header_size_delta,
        &skip_file_data_ranges,
    )?;

    // 6. Copy existing file data, skipping data we're replacing
    copy_with_skips(
        &igvm_data[var_header_end..original_file_size],
        // Adjust skip ranges to be relative to file data section
        &skip_file_data_ranges
            .iter()
            .map(|(s, e)| (s - var_header_end, e - var_header_end))
            .collect::<Vec<_>>(),
        &mut output,
    );

    // 7. Append new CoRIM data and update header offsets
    if let Some(doc) = corim_document {
        let doc_file_offset = output.len() as u32;
        output.extend_from_slice(doc);

        // Update the document header with the correct file offset
        if let Some(header_pos) = new_doc_header_pos {
            let doc_header = CorimDocumentHeader {
                compatibility_mask,
                file_offset: doc_file_offset,
                size_bytes: doc.len() as u32,
                reserved: 0,
            };
            output[header_pos..header_pos + size_of::<CorimDocumentHeader>()]
                .copy_from_slice(doc_header.as_bytes());
        }
    }

    if let Some(sig) = corim_signature {
        let sig_file_offset = output.len() as u32;
        output.extend_from_slice(sig);

        // Update the signature header with the correct file offset
        if let Some(header_pos) = new_sig_header_pos {
            let sig_header = CorimSignatureHeader {
                compatibility_mask,
                file_offset: sig_file_offset,
                size_bytes: sig.len() as u32,
                reserved: 0,
            };
            output[header_pos..header_pos + size_of::<CorimSignatureHeader>()]
                .copy_from_slice(sig_header.as_bytes());
        }
    }

    // 8. Update the fixed header
    let new_total_size = output.len() as u32;
    let mut new_fixed_header = fixed_header;
    new_fixed_header.variable_header_size = new_var_header_size as u32;
    new_fixed_header.total_file_size = new_total_size;
    new_fixed_header.checksum = 0; // Will be calculated

    // Write the updated fixed header
    output[..size_of::<IGVM_FIXED_HEADER>()].copy_from_slice(new_fixed_header.as_bytes());

    // 9. Calculate and update checksum
    let checksum_range = var_header_offset + new_var_header_size;
    let checksum = calculate_crc32(&output[..checksum_range]);
    new_fixed_header.checksum = checksum;
    output[..size_of::<IGVM_FIXED_HEADER>()].copy_from_slice(new_fixed_header.as_bytes());

    let action = if existing_doc.is_some() || existing_sig.is_some() {
        "Updated"
    } else {
        "Added"
    };

    tracing::info!(
        action = action,
        original_size = original_file_size,
        new_size = new_total_size,
        removed_var_header_size = removed_var_header_size,
        removed_file_data_size = removed_file_data_size,
        document_size = corim_document.map(|d| d.len()).unwrap_or(0),
        signature_size = corim_signature.map(|s| s.len()).unwrap_or(0),
        platform = ?platform,
        "{} CoRIM headers in IGVM file",
        action
    );

    Ok(output)
}

/// Copy data from source to output, skipping specified ranges.
/// Ranges are relative to the source slice.
fn copy_with_skips(source: &[u8], skip_ranges: &[(usize, usize)], output: &mut Vec<u8>) {
    let mut pos = 0;
    let mut sorted_ranges = skip_ranges.to_vec();
    sorted_ranges.sort_by_key(|(start, _)| *start);

    for (skip_start, skip_end) in sorted_ranges {
        // Clamp to source bounds
        let skip_start = skip_start.min(source.len());
        let skip_end = skip_end.min(source.len());

        if pos < skip_start {
            output.extend_from_slice(&source[pos..skip_start]);
        }
        pos = skip_end;
    }

    // Copy remaining data after all skips
    if pos < source.len() {
        output.extend_from_slice(&source[pos..]);
    }
}

/// Update file_offset fields in variable headers, accounting for:
/// 1. Change in variable header section size
/// 2. Removed file data ranges
fn update_file_offsets_with_removals(
    var_headers: &mut [u8],
    var_header_size_delta: isize,
    removed_file_data_ranges: &[(usize, usize)],
) -> anyhow::Result<()> {
    let mut pos = 0;

    while pos + size_of::<IGVM_VHS_VARIABLE_HEADER>() <= var_headers.len() {
        let Ok((header, _)) = IGVM_VHS_VARIABLE_HEADER::read_from_prefix(&var_headers[pos..])
        else {
            break;
        };

        let content_start = pos + size_of::<IGVM_VHS_VARIABLE_HEADER>();
        let content_len = header.length as usize;

        if content_start + content_len > var_headers.len() {
            break;
        }

        // Calculate adjustment for this header's file_offset
        let adjust_offset = |old_offset: u32| -> anyhow::Result<u32> {
            let old_offset = old_offset as usize;
            if old_offset == 0 {
                return Ok(0);
            }

            // Calculate how much data was removed BEFORE this offset
            let removed_before: usize = removed_file_data_ranges
                .iter()
                .filter(|(_, end)| *end <= old_offset)
                .map(|(start, end)| end - start)
                .sum();

            // Apply adjustments
            let new_offset = (old_offset as isize + var_header_size_delta
                - removed_before as isize)
                .try_into()
                .map_err(|_| anyhow::anyhow!("File offset underflow"))?;

            Ok(new_offset)
        };

        match header.typ {
            IgvmVariableHeaderType::IGVM_VHT_PAGE_DATA => {
                if content_len >= size_of::<IGVM_VHS_PAGE_DATA>() {
                    let (mut page_data, _) =
                        IGVM_VHS_PAGE_DATA::read_from_prefix(&var_headers[content_start..])
                            .map_err(|_| {
                                anyhow::anyhow!("Failed to parse PAGE_DATA at position {}", pos)
                            })?;

                    if page_data.file_offset != 0 {
                        page_data.file_offset = adjust_offset(page_data.file_offset)?;
                        var_headers[content_start..content_start + size_of::<IGVM_VHS_PAGE_DATA>()]
                            .copy_from_slice(page_data.as_bytes());
                    }
                }
            }
            IgvmVariableHeaderType::IGVM_VHT_VP_CONTEXT => {
                if content_len >= size_of::<IGVM_VHS_VP_CONTEXT>() {
                    let (mut vp_context, _) =
                        IGVM_VHS_VP_CONTEXT::read_from_prefix(&var_headers[content_start..])
                            .map_err(|_| {
                                anyhow::anyhow!("Failed to parse VP_CONTEXT at position {}", pos)
                            })?;

                    if vp_context.file_offset != 0 {
                        vp_context.file_offset = adjust_offset(vp_context.file_offset)?;
                        var_headers
                            [content_start..content_start + size_of::<IGVM_VHS_VP_CONTEXT>()]
                            .copy_from_slice(vp_context.as_bytes());
                    }
                }
            }
            IgvmVariableHeaderType::IGVM_VHT_PARAMETER_AREA => {
                if content_len >= size_of::<IGVM_VHS_PARAMETER_AREA>() {
                    let (mut param_area, _) =
                        IGVM_VHS_PARAMETER_AREA::read_from_prefix(&var_headers[content_start..])
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "Failed to parse PARAMETER_AREA at position {}",
                                    pos
                                )
                            })?;

                    if param_area.file_offset != 0 {
                        param_area.file_offset = adjust_offset(param_area.file_offset)?;
                        var_headers
                            [content_start..content_start + size_of::<IGVM_VHS_PARAMETER_AREA>()]
                            .copy_from_slice(param_area.as_bytes());
                    }
                }
            }
            IgvmVariableHeaderType::IGVM_VHT_CORIM_DOCUMENT => {
                if content_len >= size_of::<CorimDocumentHeader>() {
                    let (mut doc_header, _) =
                        CorimDocumentHeader::read_from_prefix(&var_headers[content_start..])
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "Failed to parse CORIM_DOCUMENT at position {}",
                                    pos
                                )
                            })?;

                    if doc_header.file_offset != 0 {
                        doc_header.file_offset = adjust_offset(doc_header.file_offset)?;
                        var_headers
                            [content_start..content_start + size_of::<CorimDocumentHeader>()]
                            .copy_from_slice(doc_header.as_bytes());
                    }
                }
            }
            IgvmVariableHeaderType::IGVM_VHT_CORIM_SIGNATURE => {
                if content_len >= size_of::<CorimSignatureHeader>() {
                    let (mut sig_header, _) =
                        CorimSignatureHeader::read_from_prefix(&var_headers[content_start..])
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "Failed to parse CORIM_SIGNATURE at position {}",
                                    pos
                                )
                            })?;

                    if sig_header.file_offset != 0 {
                        sig_header.file_offset = adjust_offset(sig_header.file_offset)?;
                        var_headers
                            [content_start..content_start + size_of::<CorimSignatureHeader>()]
                            .copy_from_slice(sig_header.as_bytes());
                    }
                }
            }
            _ => {}
        }

        let entry_size = align_to_8(size_of::<IGVM_VHS_VARIABLE_HEADER>() + content_len);
        pos += entry_size;
    }

    Ok(())
}

/// Align a size to 8-byte boundary
fn align_to_8(size: usize) -> usize {
    (size + 7) & !7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_to_8() {
        assert_eq!(align_to_8(0), 0);
        assert_eq!(align_to_8(1), 8);
        assert_eq!(align_to_8(7), 8);
        assert_eq!(align_to_8(8), 8);
        assert_eq!(align_to_8(9), 16);
        assert_eq!(align_to_8(16), 16);
    }

    #[test]
    fn test_header_sizes() {
        // Ensure our header structures match expected sizes
        assert_eq!(size_of::<CorimDocumentHeader>(), 16);
        assert_eq!(size_of::<CorimSignatureHeader>(), 16);
    }
}
